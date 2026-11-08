//! A small ECS voxel engine.
//!
//! - [`VoxelModelSource`] points an entity at a `.vox.ron` asset.
//! - When that asset loads (or changes on disk) the entity gets a [`VoxelModel`],
//!   the editable runtime data: grid, palette, scale.
//! - Whenever [`VoxelModel`] changes, its chunk meshes and lights are rebuilt as children.
//!   Meshing runs on the async compute pool; the old children stay until the new ones
//!   are ready, so a change never flashes a hole.
//! - A [`Dormant`] model keeps its data but no meshes or lights, for builds out of view.
//!
//! Gameplay code can mutate `VoxelModel::grid` directly and the change shows up a frame
//! or two later.

pub mod asset;
pub mod mesher;
pub mod query;

use std::sync::Arc;

use bevy::{
    light::NotShadowCaster,
    mesh::MeshVertexBufferLayoutRef,
    pbr::{ExtendedMaterial, MaterialExtension, MaterialExtensionKey, MaterialExtensionPipeline},
    platform::collections::{HashMap, HashSet},
    prelude::*,
    render::render_resource::{AsBindGroup, RenderPipelineDescriptor, SpecializedMeshPipelineError},
    shader::{ShaderDefVal, ShaderRef},
    tasks::{AsyncComputeTaskPool, Task, futures::check_ready},
};

pub use asset::VoxelModelAsset;
pub use cubewish_rules::voxel::{AIR, ModelData, ModelLight, ModelPart, Palette, Surface, VoxelGrid, VoxelId, noise};

/// Voxels are drawn with the standard PBR material plus [`Bevel`].
pub type VoxelMaterial = ExtendedMaterial<StandardMaterial, Bevel>;

/// Softly rounded outside edges, drawn by bending normals in `voxel_bevel.wesl`; the
/// mesher marks which edges of each face turn away from it. The world's rules set the
/// width ([`cubewish_rules::settings::Look`]); it starts sharp.
///
/// It also draws the mesher's packed vertices: `voxel_vertex.wesl` unpacks them in
/// every pass, into the fields Bevy's shaders expect from a full vertex layout.
#[derive(Asset, AsBindGroup, Reflect, Debug, Clone, Default)]
pub struct Bevel {
    /// x: the fillet width, as a share of a voxel; the rest is padding for WebGL2.
    #[uniform(100)]
    settings: Vec4,
}

impl Bevel {
    pub fn width(&self) -> f32 {
        self.settings.x
    }

    pub fn set_width(&mut self, width: f32) {
        self.settings.x = width;
    }
}

impl MaterialExtension for Bevel {
    fn vertex_shader() -> ShaderRef {
        "shaders/voxel_vertex.wesl".into()
    }

    fn prepass_vertex_shader() -> ShaderRef {
        "shaders/voxel_vertex.wesl".into()
    }

    fn deferred_vertex_shader() -> ShaderRef {
        "shaders/voxel_vertex.wesl".into()
    }

    fn fragment_shader() -> ShaderRef {
        "shaders/voxel_bevel.wesl".into()
    }

    fn specialize(
        _pipeline: &MaterialExtensionPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        layout: &MeshVertexBufferLayoutRef,
        _key: MaterialExtensionKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        descriptor.vertex.buffers = vec![layout.0.get_layout(&[mesher::ATTRIBUTE_VOXEL.at_shader_location(0)])?];
        // Bevy picks these from the mesh's attributes, which it can't see inside the
        // packed words; they switch on the vertex outputs the shaders read. The same
        // ones a full layout gets: the prepass wants normals only when it writes them.
        let has = |name: &'static str| descriptor.vertex.shader_defs.contains(&name.into());
        let mut defs: Vec<ShaderDefVal> =
            ["VERTEX_UVS", "VERTEX_UVS_A", "VERTEX_UVS_B", "VERTEX_COLORS"].map(Into::into).into();
        if !has("PREPASS_PIPELINE") || has("NORMAL_PREPASS_OR_DEFERRED_PREPASS") {
            defs.extend(["VERTEX_NORMALS", "VERTEX_TANGENTS"].map(Into::into));
        }
        descriptor.vertex.shader_defs.extend(defs.iter().cloned());
        if let Some(fragment) = &mut descriptor.fragment {
            fragment.shader_defs.extend(defs);
        }
        Ok(())
    }
}

/// How the game renders a [`Surface`]: one [`VoxelMaterial`] per surface, with
/// voxel colors from vertex colors.
pub trait SurfaceMaterial {
    fn to_material(&self) -> VoxelMaterial;
}

impl SurfaceMaterial for Surface {
    fn to_material(&self) -> VoxelMaterial {
        let base = StandardMaterial {
            base_color: Color::srgba(1.0, 1.0, 1.0, self.alpha),
            perceptual_roughness: self.roughness,
            metallic: self.metallic,
            reflectance: self.reflectance,
            emissive: self.emissive,
            alpha_mode: if self.is_transparent() { AlphaMode::Blend } else { AlphaMode::Opaque },
            ..default()
        };
        ExtendedMaterial { base, extension: Bevel::default() }
    }
}

pub struct VoxelPlugin;

impl Plugin for VoxelPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(MaterialPlugin::<VoxelMaterial>::default())
            .init_asset::<VoxelModelAsset>()
            .init_asset_loader::<asset::VoxelModelLoader>()
            .init_resource::<SharedMaterials>()
            .add_systems(Update, (sync_from_asset, put_to_sleep, start_rebuilds, finish_rebuilds).chain());
    }
}

/// Loads a [`VoxelModel`] onto this entity from a `.vox.ron` asset, and reloads it on change.
#[derive(Component)]
#[require(Transform, Visibility)]
pub struct VoxelModelSource(pub Handle<VoxelModelAsset>);

/// Runtime voxel data for one model. Voxel `(x, y, z)` occupies the cube from
/// `(x, y, z) * voxel_size` to `(x + 1, y + 1, z + 1) * voxel_size`, offset by `-pivot`.
#[derive(Component, Clone)]
#[require(Transform, Visibility)]
pub struct VoxelModel {
    pub grid: VoxelGrid,
    pub palette: Arc<Palette>,
    pub voxel_size: f32,
    /// Voxel-space point placed at the entity's origin.
    pub pivot: Vec3,
    pub lights: Vec<ModelLight>,
}

impl From<&VoxelModelAsset> for VoxelModel {
    fn from(a: &VoxelModelAsset) -> Self {
        Self::from(&a.0)
    }
}

impl From<&ModelData> for VoxelModel {
    fn from(a: &ModelData) -> Self {
        Self {
            grid: a.grid.clone(),
            palette: a.palette.clone(),
            voxel_size: a.voxel_size,
            pivot: a.pivot,
            lights: a.lights.clone(),
        }
    }
}

impl VoxelModel {
    /// Cuts `part` out of `asset` as a standalone model pivoting around the part's joint.
    pub fn from_part(asset: &ModelData, part: &ModelPart) -> Self {
        let size = (part.max - part.min).max(IVec3::ZERO);
        let mut grid = VoxelGrid::new(size.as_uvec3());
        for z in 0..size.z {
            for y in 0..size.y {
                for x in 0..size.x {
                    let p = IVec3::new(x, y, z);
                    grid.set(p, asset.grid.get(part.min + p));
                }
            }
        }
        Self {
            grid,
            palette: asset.palette.clone(),
            voxel_size: asset.voxel_size,
            pivot: part.joint - part.min.as_vec3(),
            lights: Vec::new(),
        }
    }
}

/// Pushes a model's depth toward the camera so it wins ties with coplanar faces of
/// other geometry (e.g. a build resting exactly on terrain), instead of z-fighting.
/// Positive values bias toward the camera; see [`StandardMaterial::depth_bias`].
#[derive(Component, Clone, Copy)]
pub struct DepthBias(pub f32);

/// Child entities generated from a [`VoxelModel`], replaced on every rebuild.
#[derive(Component, Default)]
struct VoxelModelParts(Vec<Entity>);

/// A model's meshes being built in the background. Replacing it (the model changed
/// again) or removing it drops the task, which cancels it.
#[derive(Component)]
pub struct Meshing(Task<Vec<mesher::ChunkMesh>>);

/// Keeps a model as data only: no meshes or lights, so it costs nothing to draw. Its
/// grid still answers collision and selection. Removing it builds the meshes again.
#[derive(Component)]
pub struct Dormant;

/// One material per distinct surface and depth bias, shared by every model that uses
/// it, so their chunks draw in the same batches. Kept for the whole session; a world
/// holds only so many distinct surfaces.
#[derive(Resource, Default)]
struct SharedMaterials(HashMap<[u32; 9], Handle<VoxelMaterial>>);

impl SharedMaterials {
    fn get(&mut self, materials: &mut Assets<VoxelMaterial>, surface: &Surface, bias: f32) -> Handle<VoxelMaterial> {
        let e = surface.emissive;
        let key = [surface.roughness, surface.metallic, surface.reflectance, e.red, e.green, e.blue, e.alpha, surface.alpha, bias]
            .map(f32::to_bits);
        self.0
            .entry(key)
            .or_insert_with(|| {
                let mut material = surface.to_material();
                material.base.depth_bias = bias;
                materials.add(material)
            })
            .clone()
    }
}

/// Marks a generated chunk mesh.
#[derive(Component)]
pub struct VoxelChunk;

fn sync_from_asset(
    mut commands: Commands,
    mut events: MessageReader<AssetEvent<VoxelModelAsset>>,
    assets: Res<Assets<VoxelModelAsset>>,
    sources: Query<(Entity, &VoxelModelSource, Has<VoxelModel>)>,
) {
    let changed: HashSet<_> = events
        .read()
        .filter_map(|e| match e {
            AssetEvent::Modified { id } | AssetEvent::LoadedWithDependencies { id } => Some(*id),
            _ => None,
        })
        .collect();
    for (entity, source, has_model) in &sources {
        if has_model && !changed.contains(&source.0.id()) {
            continue;
        }
        if let Some(asset) = assets.get(&source.0) {
            commands.entity(entity).insert(VoxelModel::from(asset));
        }
    }
}

/// Drops the meshes and lights of models that just went dormant.
fn put_to_sleep(mut commands: Commands, mut models: Query<(Entity, Option<&mut VoxelModelParts>), Added<Dormant>>) {
    for (entity, parts) in &mut models {
        for e in parts.map(|mut p| std::mem::take(&mut p.0)).unwrap_or_default() {
            commands.entity(e).despawn();
        }
        commands.entity(entity).remove::<Meshing>();
    }
}

/// Starts meshing every changed model, and every model that just woke up.
fn start_rebuilds(
    mut commands: Commands,
    mut woken: RemovedComponents<Dormant>,
    changed: Query<Entity, (Changed<VoxelModel>, Without<Dormant>)>,
    models: Query<&VoxelModel, Without<Dormant>>,
) {
    let woken: HashSet<Entity> = woken.read().collect();
    for entity in changed.iter().chain(woken.into_iter()) {
        let Ok(model) = models.get(entity) else {
            continue;
        };
        let (grid, palette) = (model.grid.clone(), model.palette.clone());
        let task = AsyncComputeTaskPool::get().spawn(async move { mesher::mesh_grid(&grid, &palette) });
        commands.entity(entity).try_insert(Meshing(task));
    }
}

/// Swaps in the meshes of finished tasks, with the model's lights.
fn finish_rebuilds(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<VoxelMaterial>>,
    mut shared: ResMut<SharedMaterials>,
    mut models: Query<(Entity, &VoxelModel, &mut Meshing, Option<&mut VoxelModelParts>, Option<&DepthBias>)>,
) {
    for (entity, model, mut meshing, parts, bias) in &mut models {
        let Some(chunks) = check_ready(&mut meshing.0) else {
            continue;
        };
        commands.entity(entity).remove::<Meshing>();
        for e in parts.map(|mut p| std::mem::take(&mut p.0)).unwrap_or_default() {
            commands.entity(e).despawn();
        }

        let bias = bias.map_or(0.0, |b| b.0);
        let offset = -model.pivot * model.voxel_size;
        let mut spawned = Vec::with_capacity(chunks.len() + model.lights.len());
        for chunk in chunks {
            let surface = &model.palette.surfaces[chunk.surface];
            let mut e = commands.spawn((
                Name::new(format!("Voxel chunk {} ({})", chunk.origin, surface.name)),
                VoxelChunk,
                Mesh3d(meshes.add(chunk.mesh)),
                MeshMaterial3d(shared.get(&mut materials, surface, bias)),
                Transform::from_translation(offset + chunk.origin.as_vec3() * model.voxel_size)
                    .with_scale(Vec3::splat(model.voxel_size)),
                ChildOf(entity),
            ));
            if surface.is_transparent() {
                e.insert(NotShadowCaster);
            }
            spawned.push(e.id());
        }
        for light in &model.lights {
            spawned.push(
                commands
                    .spawn((
                        Name::new("Voxel light"),
                        PointLight {
                            color: light.color,
                            intensity: light.intensity,
                            range: light.range,
                            radius: model.voxel_size * 2.0,
                            // Lights usually sit inside emissive voxels, which would
                            // otherwise shadow them completely.
                            shadow_maps_enabled: false,
                            ..default()
                        },
                        Transform::from_translation(offset + light.position * model.voxel_size),
                        ChildOf(entity),
                    ))
                    .id(),
            );
        }
        commands.entity(entity).insert(VoxelModelParts(spawned));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<VoxelMaterial>>()
            .init_resource::<SharedMaterials>()
            .add_systems(Update, (put_to_sleep, start_rebuilds, finish_rebuilds).chain());
        app
    }

    fn model() -> VoxelModel {
        let mut grid = VoxelGrid::new(UVec3::splat(4));
        grid.set(IVec3::ONE, 1);
        let palette = crate::world::terrain::palette();
        VoxelModel { grid, palette: Arc::new(palette), voxel_size: 0.25, pivot: Vec3::ZERO, lights: Vec::new() }
    }

    /// Runs frames until the background meshing is done.
    fn settle(app: &mut App, entity: Entity) {
        for _ in 0..1000 {
            app.update();
            if app.world().get::<Meshing>(entity).is_none() {
                return;
            }
            std::thread::yield_now();
        }
        panic!("meshing never finished");
    }

    fn parts(app: &App, entity: Entity) -> usize {
        app.world().get::<VoxelModelParts>(entity).map_or(0, |p| p.0.len())
    }

    #[test]
    fn meshes_in_the_background_and_sleeps() {
        let mut app = app();
        let entity = app.world_mut().spawn(model()).id();
        settle(&mut app, entity);
        assert_eq!(parts(&app, entity), 1);

        app.world_mut().entity_mut(entity).insert(Dormant);
        app.update();
        assert_eq!(parts(&app, entity), 0);

        app.world_mut().entity_mut(entity).remove::<Dormant>();
        settle(&mut app, entity);
        assert_eq!(parts(&app, entity), 1);
    }

    #[test]
    fn models_share_materials() {
        let mut app = app();
        let a = app.world_mut().spawn(model()).id();
        let b = app.world_mut().spawn(model()).id();
        settle(&mut app, a);
        settle(&mut app, b);
        assert_eq!(app.world().resource::<Assets<VoxelMaterial>>().len(), 1);
    }
}
