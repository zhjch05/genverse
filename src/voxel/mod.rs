//! A small ECS voxel engine.
//!
//! - [`VoxelModelSource`] points an entity at a `.vox.ron` asset.
//! - When that asset loads (or changes on disk) the entity gets a [`VoxelModel`],
//!   the editable runtime data: grid, palette, scale.
//! - Whenever [`VoxelModel`] changes, its chunk meshes and lights are rebuilt as children.
//!
//! Gameplay code can mutate `VoxelModel::grid` directly and the change shows up next frame.

mod asset;
mod grid;
mod mesher;
mod noise;
mod palette;

use std::sync::Arc;

use bevy::{light::NotShadowCaster, platform::collections::HashSet, prelude::*};

pub use asset::{ModelLight, VoxelModelAsset};
pub use grid::VoxelGrid;
pub use palette::Palette;

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
        Self {
            grid: a.grid.clone(),
            palette: a.palette.clone(),
            voxel_size: a.voxel_size,
            pivot: a.pivot,
            lights: a.lights.clone(),
        }
    }
}

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
    mut models: Query<(Entity, &VoxelModel, Option<&mut VoxelModelParts>), Changed<VoxelModel>>,
) {
    for (entity, model, parts) in &mut models {
        let started = bevy::platform::time::Instant::now();
        let old = parts.map(|mut p| std::mem::take(&mut p.0)).unwrap_or_default();
        for e in old {
            commands.entity(e).despawn();
        }

        let surface_materials: Vec<_> = model
            .palette
            .surfaces
            .iter()
            .map(|s| materials.add(s.to_material()))
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
