//! An endless, streamed voxel world.
//!
//! Chunks around every [`ChunkLoader`] are generated and meshed on the async compute
//! pool, then spawned as mesh entities. Each loaded chunk keeps its voxels (with a
//! one-voxel border copied from its neighbors) for collision queries.

pub mod terrain;

use std::sync::Arc;

use bevy::{
    light::NotShadowCaster,
    platform::collections::HashMap,
    prelude::*,
    tasks::{AsyncComputeTaskPool, Task, futures::check_ready},
};

use crate::voxel::{
    Palette, VoxelGrid, VoxelId,
    mesher::mesh_region,
    query::{Hit, march},
};
use terrain::{Terrain, VOXEL_SIZE, WORLD_HEIGHT};

pub const CHUNK: i32 = 32;
/// Horizontal view distance, in chunks.
pub const VIEW_RADIUS: i32 = 7;
const VERTICAL_CHUNKS: i32 = WORLD_HEIGHT / CHUNK;
const MAX_IN_FLIGHT: usize = if cfg!(target_arch = "wasm32") { 2 } else { 12 };

pub struct WorldPlugin;

impl Plugin for WorldPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PreStartup, setup_world)
            .add_systems(Update, (stream_chunks, finish_chunks).chain());
    }
}

/// Keeps the world loaded around this entity.
#[derive(Component)]
pub struct ChunkLoader;

#[derive(Component)]
pub struct WorldChunk;

/// A [`VoxelModel`](crate::voxel::VoxelModel) the hero collides with, like a placed build.
#[derive(Component)]
pub struct SolidModel;

struct LoadedChunk {
    /// `(CHUNK + 2)^3` voxels: the chunk plus a one-voxel border.
    voxels: Arc<VoxelGrid>,
    entities: Vec<Entity>,
}

struct BuiltChunk {
    voxels: VoxelGrid,
    meshes: Vec<(usize, Mesh)>,
}

#[derive(Resource)]
pub struct VoxelWorld {
    pub terrain: Terrain,
    palette: Arc<Palette>,
    materials: Vec<Handle<StandardMaterial>>,
    loaded: HashMap<IVec3, LoadedChunk>,
    pending: HashMap<IVec3, Task<BuiltChunk>>,
}

impl VoxelWorld {
    pub fn voxel_at(pos: Vec3) -> IVec3 {
        (pos / VOXEL_SIZE).floor().as_ivec3()
    }

    /// The voxel at world voxel coordinate `p`, or `None` if its chunk isn't loaded.
    pub fn get(&self, p: IVec3) -> Option<VoxelId> {
        if p.y >= WORLD_HEIGHT {
            return Some(crate::voxel::AIR);
        }
        let chunk = p.div_euclid(IVec3::splat(CHUNK));
        let chunk = chunk.with_y(chunk.y.clamp(0, VERTICAL_CHUNKS - 1));
        let local = p - chunk * CHUNK + IVec3::ONE;
        self.loaded.get(&chunk).map(|c| c.voxels.get(local))
    }

    /// The first solid terrain voxel hit by `ray` within `max` meters. Stops at
    /// unloaded chunks.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))] // Used by the desktop-only builder.
    pub fn raycast(&self, ray: Ray3d, max: f32) -> Option<Hit> {
        let origin = ray.origin / VOXEL_SIZE;
        let dir = *ray.direction / VOXEL_SIZE;
        let (distance, normal) =
            march(origin, dir, 0.0, max, |cell| self.get(cell).map(terrain::is_solid))?;
        Some(Hit { distance, normal: normal.as_vec3() })
    }

    /// Whether every chunk in the column containing `pos` is loaded.
    pub fn column_loaded(&self, pos: Vec3) -> bool {
        let c = Self::voxel_at(pos).div_euclid(IVec3::splat(CHUNK));
        (0..VERTICAL_CHUNKS).all(|y| self.loaded.contains_key(&c.with_y(y)))
    }
}

fn setup_world(mut commands: Commands, mut materials: ResMut<Assets<StandardMaterial>>) {
    let palette = terrain::palette();
    let materials = palette
        .surfaces
        .iter()
        .map(|s| materials.add(s.to_material()))
        .collect();
    commands.insert_resource(VoxelWorld {
        terrain: Terrain { seed: 1337 },
        palette: Arc::new(palette),
        materials,
        loaded: default(),
        pending: default(),
    });
}

fn stream_chunks(
    mut commands: Commands,
    mut world: ResMut<VoxelWorld>,
    loaders: Query<&GlobalTransform, With<ChunkLoader>>,
) {
    let centers: Vec<IVec2> = loaders
        .iter()
        .map(|t| {
            let c = VoxelWorld::voxel_at(t.translation()).div_euclid(IVec3::splat(CHUNK));
            IVec2::new(c.x, c.z)
        })
        .collect();
    let in_range = |c: IVec3, radius: i32| {
        centers
            .iter()
            .any(|center| (IVec2::new(c.x, c.z) - center).length_squared() <= radius * radius)
    };

    // Unload with some hysteresis so chunks don't flicker at the boundary.
    let far: Vec<IVec3> = world
        .loaded
        .keys()
        .copied()
        .filter(|c| !in_range(*c, VIEW_RADIUS + 2))
        .collect();
    for c in far {
        if let Some(chunk) = world.loaded.remove(&c) {
            for e in chunk.entities {
                commands.entity(e).despawn();
            }
        }
    }

    // Queue the nearest missing chunks.
    let mut wanted = Vec::new();
    for center in &centers {
        for dz in -VIEW_RADIUS..=VIEW_RADIUS {
            for dx in -VIEW_RADIUS..=VIEW_RADIUS {
                if dx * dx + dz * dz > VIEW_RADIUS * VIEW_RADIUS {
                    continue;
                }
                for cy in 0..VERTICAL_CHUNKS {
                    let c = IVec3::new(center.x + dx, cy, center.y + dz);
                    if !world.loaded.contains_key(&c) && !world.pending.contains_key(&c) {
                        wanted.push((dx * dx + dz * dz, c));
                    }
                }
            }
        }
    }
    wanted.sort_by_key(|(d, _)| *d);

    let pool = AsyncComputeTaskPool::get();
    for (_, c) in wanted {
        if world.pending.len() >= MAX_IN_FLIGHT {
            break;
        }
        let terrain = world.terrain;
        let palette = world.palette.clone();
        let task = pool.spawn(async move { build_chunk(terrain, &palette, c) });
        world.pending.insert(c, task);
    }
}

fn build_chunk(terrain: Terrain, palette: &Palette, chunk: IVec3) -> BuiltChunk {
    let origin = chunk * CHUNK;
    let padded_min = origin - IVec3::ONE;
    let voxels = terrain.generate(padded_min, UVec3::splat((CHUNK + 2) as u32));
    let meshes = mesh_region(
        |p| voxels.get(p - padded_min),
        palette,
        VOXEL_SIZE,
        origin,
        origin + IVec3::splat(CHUNK),
    );
    BuiltChunk { voxels, meshes }
}

fn finish_chunks(
    mut commands: Commands,
    mut world: ResMut<VoxelWorld>,
    mut meshes: ResMut<Assets<Mesh>>,
) {
    let world = &mut *world;
    let mut done = Vec::new();
    for (c, task) in world.pending.iter_mut() {
        if let Some(built) = check_ready(task) {
            done.push((*c, built));
        }
    }
    for (c, built) in done {
        world.pending.remove(&c);
        let translation = (c * CHUNK).as_vec3() * VOXEL_SIZE;
        let entities = built
            .meshes
            .into_iter()
            .map(|(surface, mesh)| {
                let mut e = commands.spawn((
                    WorldChunk,
                    Mesh3d(meshes.add(mesh)),
                    MeshMaterial3d(world.materials[surface].clone()),
                    Transform::from_translation(translation),
                ));
                if world.palette.surfaces[surface].is_transparent() {
                    e.insert(NotShadowCaster);
                }
                e.id()
            })
            .collect();
        world.loaded.insert(
            c,
            LoadedChunk {
                voxels: Arc::new(built.voxels),
                entities,
            },
        );
    }
}
