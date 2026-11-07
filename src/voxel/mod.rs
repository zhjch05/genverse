//! A small ECS voxel engine.
//!
//! - [`VoxelModelSource`] points an entity at a `.vox.ron` asset.
//! - When that asset loads (or changes on disk) the entity gets a [`VoxelModel`],
//!   the editable runtime data: grid, palette, scale.
//! - Whenever [`VoxelModel`] changes, its chunk meshes and lights are rebuilt as children.
//!
//! Gameplay code can mutate `VoxelModel::grid` directly and the change shows up next frame.

pub mod asset;
pub mod mesher;
pub mod query;

use std::sync::Arc;

use bevy::{light::NotShadowCaster, platform::collections::HashSet, prelude::*};

pub use asset::VoxelModelAsset;
pub use cubelark_rules::voxel::{AIR, ModelData, ModelLight, ModelPart, Palette, Surface, VoxelGrid, VoxelId, noise};

/// How the game renders a [`Surface`]: one [`StandardMaterial`] per surface, with
/// voxel colors from vertex colors.
pub trait SurfaceMaterial {
    fn to_material(&self) -> StandardMaterial;
}

impl SurfaceMaterial for Surface {
    fn to_material(&self) -> StandardMaterial {
        StandardMaterial {
            base_color: Color::srgba(1.0, 1.0, 1.0, self.alpha),
            perceptual_roughness: self.roughness,
            metallic: self.metallic,
            reflectance: self.reflectance,
            emissive: self.emissive,
            alpha_mode: if self.is_transparent() { AlphaMode::Blend } else { AlphaMode::Opaque },
            ..default()
        }
    }
}

pub struct VoxelPlugin;

impl Plugin for VoxelPlugin {
    fn build(&self, app: &mut App) {
        app.init_asset::<VoxelModelAsset>()
            .init_asset_loader::<asset::VoxelModelLoader>()
            .add_systems(Update, (sync_from_asset, rebuild_models).chain());
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

fn rebuild_models(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut models: Query<
        (Entity, &VoxelModel, Option<&mut VoxelModelParts>, Option<&DepthBias>),
        Changed<VoxelModel>,
    >,
) {
    for (entity, model, parts, bias) in &mut models {
        let started = bevy::platform::time::Instant::now();
        let old = parts.map(|mut p| std::mem::take(&mut p.0)).unwrap_or_default();
        for e in old {
            commands.entity(e).despawn();
        }

        let surface_materials: Vec<_> = model
            .palette
            .surfaces
            .iter()
            .map(|s| {
                let mut material = s.to_material();
                material.depth_bias = bias.map_or(0.0, |b| b.0);
                materials.add(material)
            })
            .collect();
        let offset = -model.pivot * model.voxel_size;
        let chunks = mesher::mesh_grid(&model.grid, &model.palette, model.voxel_size);
        let chunk_count = chunks.len();

        let mut spawned = Vec::with_capacity(chunk_count + model.lights.len());
        for chunk in chunks {
            let surface = &model.palette.surfaces[chunk.surface];
            let mut e = commands.spawn((
                Name::new(format!("Voxel chunk {} ({})", chunk.origin, surface.name)),
                VoxelChunk,
                Mesh3d(meshes.add(chunk.mesh)),
                MeshMaterial3d(surface_materials[chunk.surface].clone()),
                Transform::from_translation(offset + chunk.origin.as_vec3() * model.voxel_size),
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
        info!(
            "rebuilt voxel model {entity}: {chunk_count} chunk meshes in {:.0?}",
            started.elapsed()
        );
    }
}
