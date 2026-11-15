//! A small ECS voxel engine.
//!
//! - [`VoxelModelSource`] points an entity at a `.vox.ron` asset.
//! - When that asset loads (or changes on disk) the entity gets a [`VoxelModel`],
//!   the editable runtime data: grid, palette, scale.
//! - Whenever [`VoxelModel`] changes, its chunk meshes and lights are rebuilt as children.
//!   Meshing runs on the async compute pool; the old children stay until the new ones
//!   are ready, so a change never flashes a hole.
//! - A [`Dormant`] model keeps its data but no meshes or lights, for builds out of view.
//! - A [`Coarse`] model is drawn at half resolution without its lights, for builds
//!   farther off.
//!
//! - A model with a [`Reveal`] shows its next meshes voxel by voxel (`reveal.rs`).
//!
//! Gameplay code can mutate `VoxelModel::grid` directly and the change shows up a frame
//! or two later.

pub mod asset;
pub mod mesher;
pub mod query;
pub mod reveal;

use std::sync::Arc;

use bevy::{
    asset::{RenderAssetUsages, uuid_handle},
    light::NotShadowCaster,
    mesh::{MeshTag, MeshVertexBufferLayoutRef},
    pbr::{ExtendedMaterial, MaterialExtension, MaterialExtensionKey, MaterialExtensionPipeline},
    platform::collections::{HashMap, HashSet},
    prelude::*,
    render::{
        render_resource::{AsBindGroup, RenderPipelineDescriptor, SpecializedMeshPipelineError},
        storage::ShaderBuffer,
    },
    shader::{ShaderDefVal, ShaderRef},
    tasks::{AsyncComputeTaskPool, Task, futures::check_ready},
};

pub use asset::VoxelModelAsset;
pub use genverse_rules::voxel::{
    AIR, ModelData, ModelLight, ModelPart, Palette, Surface, VoxelGrid, VoxelId, noise,
};
pub use reveal::{Reveal, Revealing, Reveals};

/// Voxels are drawn with the standard PBR material plus [`Bevel`].
pub type VoxelMaterial = ExtendedMaterial<StandardMaterial, Bevel>;

/// Softly rounded outside edges, drawn by bending normals in `voxel_bevel.wesl`; the
/// mesher marks which edges of each face turn away from it. The world's rules set the
/// width ([`genverse_rules::settings::Look`]); it starts sharp.
///
/// It also draws the mesher's packed vertices: `voxel_vertex.wesl` unpacks them in
/// every pass, into the fields Bevy's shaders expect from a full vertex layout.
#[derive(Asset, AsBindGroup, Reflect, Debug, Clone)]
pub struct Bevel {
    /// x: the fillet width, as a share of a voxel; the rest is padding for WebGL2.
    #[uniform(100)]
    settings: Vec4,
    /// Where the camera and hero are, for the cutout (`voxel_cutout.wesl`): one buffer
    /// every voxel material shares, [`CUTOUT_BUFFER`].
    #[storage(101, read_only)]
    cutout: Handle<ShaderBuffer>,
    /// The reveals running now (`voxel_vertex.wesl`), shared the same way.
    #[storage(102, read_only)]
    reveal: Handle<ShaderBuffer>,
}

impl Default for Bevel {
    fn default() -> Self {
        Self {
            settings: Vec4::ZERO,
            cutout: CUTOUT_BUFFER,
            reveal: reveal::REVEAL_BUFFER,
        }
    }
}

/// The buffer holding the camera's cutout, rewritten in place as the camera moves.
///
/// It lives outside the materials on purpose: changing a material marks every mesh
/// drawn with it as changed, so Bevy re-extracts and re-specializes every chunk.
/// Rewriting a buffer of the same size only uploads its 32 bytes.
const CUTOUT_BUFFER: Handle<ShaderBuffer> = uuid_handle!("6f1c2b7e-5a43-4d8e-9b61-2c0e7f4a9d15");

/// Dithers away whatever voxels stand between the camera and the hero, so the hero
/// shows through what the camera doesn't move in front of. Set by the camera each
/// frame; `strength` 0 turns it off.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq)]
pub struct CameraCutout {
    pub camera: Vec3,
    pub hero: Vec3,
    pub radius: f32,
    pub strength: f32,
}

impl CameraCutout {
    /// As `voxel_cutout.wesl` reads it: camera xyz and strength, then hero xyz and radius.
    fn buffer(&self) -> ShaderBuffer {
        let data = [
            self.camera.extend(self.strength),
            self.hero.extend(self.radius),
        ]
        .map(|v| v.to_array())
        .concat();
        ShaderBuffer::new(data, RenderAssetUsages::RENDER_WORLD)
    }
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

    fn prepass_fragment_shader() -> ShaderRef {
        "shaders/voxel_prepass.wesl".into()
    }

    fn specialize(
        _pipeline: &MaterialExtensionPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        layout: &MeshVertexBufferLayoutRef,
        _key: MaterialExtensionKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        descriptor.vertex.buffers = vec![
            layout
                .0
                .get_layout(&[mesher::ATTRIBUTE_VOXEL.at_shader_location(0)])?,
        ];
        // Bevy picks these from the mesh's attributes, which it can't see inside the
        // packed words; they switch on the vertex outputs the shaders read. The same
        // ones a full layout gets: the prepass wants normals only when it writes them.
        let has = |name: &'static str| descriptor.vertex.shader_defs.contains(&name.into());
        let mut defs: Vec<ShaderDefVal> = [
            "VERTEX_UVS",
            "VERTEX_UVS_A",
            "VERTEX_UVS_B",
            "VERTEX_COLORS",
        ]
        .map(Into::into)
        .into();
        if !has("PREPASS_PIPELINE") || has("NORMAL_PREPASS_OR_DEFERRED_PREPASS") {
            defs.extend(["VERTEX_NORMALS", "VERTEX_TANGENTS"].map(Into::into));
        }
        // The cutout is for what the camera sees: the main pass and the camera's
        // prepasses (which always write normals or motion), never a shadow map.
        if !has("PREPASS_PIPELINE")
            || has("NORMAL_PREPASS")
            || has("MOTION_VECTOR_PREPASS")
            || has("DEFERRED_PREPASS")
        {
            defs.push("CAMERA_CUTOUT".into());
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
            alpha_mode: if self.is_transparent() {
                AlphaMode::Blend
            } else {
                AlphaMode::Opaque
            },
            ..default()
        };
        ExtendedMaterial {
            base,
            extension: Bevel::default(),
        }
    }
}

pub struct VoxelPlugin;

impl Plugin for VoxelPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(MaterialPlugin::<VoxelMaterial>::default())
            .init_asset::<VoxelModelAsset>()
            .init_asset_loader::<asset::VoxelModelLoader>()
            .init_resource::<SharedMaterials>()
            .init_resource::<CameraCutout>()
            .add_systems(
                Update,
                (
                    sync_from_asset,
                    put_to_sleep,
                    start_rebuilds,
                    finish_rebuilds,
                    end_reveals,
                )
                    .chain(),
            )
            .add_systems(PostUpdate, (apply_cutout, reveal::tick));
        reveal::setup(app);
        app.world_mut()
            .resource_mut::<Assets<ShaderBuffer>>()
            .insert(&CUTOUT_BUFFER, CameraCutout::default().buffer())
            .ok();
    }
}

/// Uploads the camera's cutout to the buffer every voxel material reads.
fn apply_cutout(cutout: Res<CameraCutout>, mut buffers: ResMut<Assets<ShaderBuffer>>) {
    if cutout.is_changed() {
        buffers.insert(&CUTOUT_BUFFER, cutout.buffer()).ok();
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
pub struct Meshing {
    task: Task<Meshed>,
    /// Whether these are the half-resolution meshes.
    coarse: bool,
}

/// A model's meshes, and with a [`Reveal`] those of what it's losing.
struct Meshed {
    chunks: Vec<mesher::ChunkMesh>,
    /// The ghost, with the palette its voxels are in, and how long the reveal takes.
    reveal: Option<(Vec<mesher::ChunkMesh>, Option<Arc<Palette>>, f32)>,
}

/// Draws a model at half resolution, each 2x2x2 voxels merged into one, and without
/// its lights (its glowing voxels still glow): about a quarter of the faces, for builds
/// too far off to show the difference. Adding or removing it rebuilds the meshes.
#[derive(Component)]
pub struct Coarse;

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
    fn get(
        &mut self,
        materials: &mut Assets<VoxelMaterial>,
        surface: &Surface,
        bias: f32,
    ) -> Handle<VoxelMaterial> {
        let e = surface.emissive;
        let key = [
            surface.roughness,
            surface.metallic,
            surface.reflectance,
            e.red,
            e.green,
            e.blue,
            e.alpha,
            surface.alpha,
            bias,
        ]
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
fn put_to_sleep(
    mut commands: Commands,
    mut models: Query<(Entity, Option<&mut VoxelModelParts>), Added<Dormant>>,
) {
    for (entity, parts) in &mut models {
        for e in parts
            .map(|mut p| std::mem::take(&mut p.0))
            .unwrap_or_default()
        {
            commands.entity(e).try_despawn();
        }
        // Out of view, there's nobody to reveal it to.
        commands
            .entity(entity)
            .remove::<(Meshing, Reveal, Revealing)>();
    }
}

/// Starts meshing every changed model, every model that just woke up, and every model
/// whose resolution changed.
fn start_rebuilds(
    mut commands: Commands,
    reveals: Res<Reveals>,
    mut woken: RemovedComponents<Dormant>,
    mut refined: RemovedComponents<Coarse>,
    changed: Query<Entity, (Or<(Changed<VoxelModel>, Added<Coarse>)>, Without<Dormant>)>,
    models: Query<(&VoxelModel, Has<Coarse>, Option<&Reveal>), Without<Dormant>>,
) {
    let others: HashSet<Entity> = woken.read().chain(refined.read()).collect();
    for entity in changed.iter().chain(others) {
        let Ok((model, coarse, reveal)) = models.get(entity) else {
            continue;
        };
        let (grid, palette) = (model.grid.clone(), model.palette.clone());
        // A reveal is for these meshes only; whatever is meshed next is plain.
        if reveal.is_some() {
            commands.entity(entity).remove::<Reveal>();
        }
        let reveal = reveal
            .filter(|r| reveals.enabled && !coarse && r.arrival.len() == grid.cells().len())
            .cloned();
        let task = AsyncComputeTaskPool::get().spawn(async move {
            let Some(reveal) = reveal else {
                let grid = if coarse { downsample(&grid) } else { grid };
                return Meshed {
                    chunks: mesher::mesh_grid(&grid, &palette),
                    reveal: None,
                };
            };
            let chunks =
                mesher::mesh_grid_fresh(&grid, Some(&|p| reveal.fresh(&grid, p)), &palette);
            let ghost = reveal.ghost.as_ref().map(|(ghost, palette)| {
                // The ghost's voxels leave in the same order; one that's replaced goes at once.
                let going = |p: IVec3| mesher::Fresh {
                    arrival: reveal.fresh(&grid, p).arrival,
                    swap: grid.get(p) != AIR,
                };
                (
                    mesher::mesh_grid_fresh(ghost, Some(&going), palette),
                    palette.clone(),
                )
            });
            let (ghosts, ghost_palette) = ghost.map_or((Vec::new(), None), |(meshes, palette)| {
                (meshes, Some(palette))
            });
            Meshed {
                chunks,
                reveal: Some((ghosts, ghost_palette, reveal.seconds)),
            }
        });
        commands.entity(entity).try_insert(Meshing { task, coarse });
    }
}

/// Merges each 2x2x2 voxels of `grid` into one: filled when at least two of them are,
/// so thin posts and walls survive, with the topmost one's material, so roofs and
/// grass keep their color.
fn downsample(grid: &VoxelGrid) -> VoxelGrid {
    let size = (grid.size() + 1) / 2;
    let mut coarse = VoxelGrid::new(size);
    for z in 0..size.z as i32 {
        for y in 0..size.y as i32 {
            for x in 0..size.x as i32 {
                let at = IVec3::new(x, y, z) * 2;
                let (mut filled, mut top) = (0, AIR);
                for dy in (0..2).rev() {
                    for dz in 0..2 {
                        for dx in 0..2 {
                            let id = grid.get(at + IVec3::new(dx, dy, dz));
                            if id != AIR {
                                filled += 1;
                                if top == AIR {
                                    top = id;
                                }
                            }
                        }
                    }
                }
                if filled >= 2 {
                    coarse.set(IVec3::new(x, y, z), top);
                }
            }
        }
    }
    coarse
}

/// Swaps in the meshes of finished tasks, with the model's lights.
fn finish_rebuilds(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<VoxelMaterial>>,
    mut shared: ResMut<SharedMaterials>,
    mut reveals: ResMut<Reveals>,
    mut models: Query<(
        Entity,
        &VoxelModel,
        &mut Meshing,
        Option<&mut VoxelModelParts>,
        Option<&DepthBias>,
        Option<&Revealing>,
    )>,
) {
    for (entity, model, mut meshing, parts, bias, revealing) in &mut models {
        let Some(meshed) = check_ready(&mut meshing.task) else {
            continue;
        };
        let coarse = meshing.coarse;
        commands.entity(entity).remove::<Meshing>();
        for e in parts
            .map(|mut p| std::mem::take(&mut p.0))
            .unwrap_or_default()
        {
            commands.entity(e).try_despawn();
        }
        // These meshes replace the ones a reveal was showing.
        if let Some(revealing) = revealing {
            reveals.close(revealing.slot);
            commands.entity(entity).remove::<Revealing>();
        }
        let Meshed { chunks, reveal } = meshed;
        // Without a free slot the meshes go untagged, and so show whole.
        let reveal = reveal.and_then(|(ghosts, palette, seconds)| {
            Some((ghosts, palette, reveals.open(seconds, true, Some(entity))?))
        });
        let slot = reveal.as_ref().map(|r| r.2);

        let bias = bias.map_or(0.0, |b| b.0);
        let offset = -model.pivot * model.voxel_size;
        let voxel = if coarse {
            model.voxel_size * 2.0
        } else {
            model.voxel_size
        };
        let mut spawned = Vec::with_capacity(chunks.len() + model.lights.len());
        let mut spawn = |commands: &mut Commands,
                         chunk: mesher::ChunkMesh,
                         palette: &Palette,
                         tag: Option<MeshTag>| {
            let surface = &palette.surfaces[chunk.surface];
            let mut e = commands.spawn((
                Name::new(format!("Voxel chunk {} ({})", chunk.origin, surface.name)),
                VoxelChunk,
                Mesh3d(meshes.add(chunk.mesh)),
                MeshMaterial3d(shared.get(&mut materials, surface, bias)),
                Transform::from_translation(offset + chunk.origin.as_vec3() * voxel)
                    .with_scale(Vec3::splat(voxel)),
                ChildOf(entity),
            ));
            if surface.is_transparent() {
                e.insert(NotShadowCaster);
            }
            if let Some(tag) = tag {
                e.insert(tag);
            }
            e.id()
        };
        for chunk in chunks {
            spawned.push(spawn(
                &mut commands,
                chunk,
                &model.palette,
                slot.map(|s| Reveals::tag(s, false)),
            ));
        }
        if let Some((ghosts, palette, slot)) = reveal {
            let palette = palette.unwrap_or_else(|| model.palette.clone());
            let ghosts: Vec<Entity> = ghosts
                .into_iter()
                .map(|chunk| {
                    spawn(
                        &mut commands,
                        chunk,
                        &palette,
                        Some(Reveals::tag(slot, true)),
                    )
                })
                .collect();
            spawned.extend(&ghosts);
            commands.entity(entity).insert(Revealing { slot, ghosts });
        }
        let lights = if coarse { &[][..] } else { &model.lights[..] };
        for light in lights {
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

/// Ends the reveals that have run their time: the ghost goes, the meshes show whole,
/// and plain ones are built to take their place.
fn end_reveals(
    mut commands: Commands,
    mut reveals: ResMut<Reveals>,
    mut models: Query<(Entity, &Revealing, &mut VoxelModel, &mut VoxelModelParts)>,
) {
    for (entity, revealing, mut model, mut parts) in &mut models {
        if !reveals.done(revealing.slot) {
            continue;
        }
        reveals.close(revealing.slot);
        for ghost in &revealing.ghosts {
            commands.entity(*ghost).try_despawn();
        }
        parts.0.retain(|e| !revealing.ghosts.contains(e));
        for part in &parts.0 {
            // The slot may go to another reveal before the plain meshes are in.
            commands.entity(*part).try_remove::<MeshTag>();
        }
        commands.entity(entity).remove::<Revealing>();
        model.set_changed();
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
            .init_resource::<Reveals>()
            .add_systems(
                Update,
                (put_to_sleep, start_rebuilds, finish_rebuilds, end_reveals).chain(),
            );
        app
    }

    fn model() -> VoxelModel {
        let mut grid = VoxelGrid::new(UVec3::splat(4));
        grid.set(IVec3::ONE, 1);
        let palette = crate::world::terrain::palette();
        VoxelModel {
            grid,
            palette: Arc::new(palette),
            voxel_size: 0.25,
            pivot: Vec3::ZERO,
            lights: Vec::new(),
        }
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
        app.world()
            .get::<VoxelModelParts>(entity)
            .map_or(0, |p| p.0.len())
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
    fn coarse_models_have_fewer_faces_and_no_lights() {
        let mut app = app();
        let mut big = model();
        big.grid = VoxelGrid::new(UVec3::splat(16));
        for z in 0..16 {
            for y in 0..16 {
                for x in 0..16 {
                    if (x + y + z) % 5 != 0 {
                        big.grid.set(IVec3::new(x, y, z), 1);
                    }
                }
            }
        }
        big.lights.push(ModelLight {
            position: Vec3::ONE,
            color: Color::WHITE,
            intensity: 1.0,
            range: 1.0,
        });
        let entity = app.world_mut().spawn(big).id();
        settle(&mut app, entity);
        let faces = |app: &App| {
            let meshes = app.world().resource::<Assets<Mesh>>();
            let parts = &app.world().get::<VoxelModelParts>(entity).unwrap().0;
            let faces: usize = parts
                .iter()
                .filter_map(|e| app.world().get::<Mesh3d>(*e))
                .map(|m| meshes.get(&m.0).unwrap().indices().unwrap().len() / 6)
                .sum();
            (faces, parts.len())
        };
        let (fine, fine_parts) = faces(&app);
        app.world_mut().entity_mut(entity).insert(Coarse);
        settle(&mut app, entity);
        let (coarse, coarse_parts) = faces(&app);
        assert!(coarse * 3 < fine, "{coarse} coarse faces vs {fine}");
        // The light is gone; the one chunk mesh stays.
        assert_eq!((fine_parts, coarse_parts), (2, 1));
    }

    /// A revealed model's meshes are tagged with its slot, and the ghost of what it
    /// lost is drawn beside them; when the time is up both give way to plain meshes.
    #[test]
    fn a_reveal_tags_its_meshes_and_ends_plain() {
        let mut app = app();
        let old = model();
        let mut new = model();
        new.grid.set(IVec3::ONE, AIR);
        new.grid.set(IVec3::new(2, 1, 1), 1);
        let reveal =
            Reveal::between((&old.grid, &old.palette), (&new.grid, &new.palette), None).unwrap();
        let entity = app.world_mut().spawn((new, reveal)).id();
        settle(&mut app, entity);
        let tags = |app: &App| -> Vec<u32> {
            let parts = &app.world().get::<VoxelModelParts>(entity).unwrap().0;
            parts
                .iter()
                .filter_map(|e| app.world().get::<MeshTag>(*e))
                .map(|t| t.value)
                .collect()
        };
        // The model's one chunk, and the ghost of the voxel taken away.
        let mut tagged = tags(&app);
        tagged.sort();
        assert_eq!(tagged, [1, 1 | 1 << 31]);
        assert!(
            app.world().get::<Revealing>(entity).is_some()
                && app.world().get::<Reveal>(entity).is_none()
        );

        app.world_mut().resource_mut::<Reveals>().skip(10.0);
        app.update();
        assert!(app.world().get::<Revealing>(entity).is_none());
        settle(&mut app, entity);
        assert_eq!((parts(&app, entity), tags(&app).len()), (1, 0));
        // The slot is free again.
        assert_eq!(
            app.world_mut()
                .resource_mut::<Reveals>()
                .open(1.0, true, None),
            Some(0)
        );
    }

    /// With the player's setting off, a reveal is dropped and the model just appears.
    #[test]
    fn reveals_can_be_switched_off() {
        let mut app = app();
        app.world_mut().resource_mut::<Reveals>().enabled = false;
        let model = model();
        let reveal = Reveal::whole(&model.grid, None);
        let entity = app.world_mut().spawn((model, reveal)).id();
        settle(&mut app, entity);
        assert!(
            app.world().get::<Revealing>(entity).is_none()
                && app.world().get::<Reveal>(entity).is_none()
        );
        let part = app.world().get::<VoxelModelParts>(entity).unwrap().0[0];
        assert!(app.world().get::<MeshTag>(part).is_none());
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
