//! An endless, streamed voxel world.
//!
//! Chunks around every [`ChunkLoader`] are generated and meshed on the async compute
//! pool, then spawned as mesh entities. Each loaded chunk keeps its voxels (with a
//! one-voxel border copied from its neighbors) for collision queries.
//!
//! Terrain changes are stored as a sparse layer on top of the seed ([`VoxelWorld::set`]).
//! They're applied whenever a chunk is (re)built, so the world stays "seed + edits":
//! cheap to save, and exactly what a server would replicate.

pub use cubelark_rules::terrain;

use std::sync::Arc;

use bevy::{
    light::NotShadowCaster,
    platform::collections::{HashMap, HashSet},
    prelude::*,
    tasks::{AsyncComputeTaskPool, Task, futures::check_ready},
};
use serde::{Deserialize, Serialize};

use crate::voxel::{
    SurfaceMaterial,
    AIR, Palette, VoxelGrid, VoxelId,
    mesher::mesh_region,
    query::{Hit, march},
};
use terrain::{Terrain, VOXEL_SIZE, WORLD_HEIGHT};

pub const CHUNK: i32 = 32;
/// Horizontal view distance, in chunks.
pub const VIEW_RADIUS: i32 = 7;
const VERTICAL_CHUNKS: i32 = WORLD_HEIGHT / CHUNK;
const MAX_IN_FLIGHT: usize = if cfg!(target_arch = "wasm32") { 2 } else { 12 };
const SEED: u32 = 1337;
#[cfg(not(target_arch = "wasm32"))]
const EDITS_PATH: &str = "saves/world_edits.json";

pub struct WorldPlugin;

impl Plugin for WorldPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PreStartup, setup_world)
            .add_systems(Update, (stream_chunks, finish_chunks, save_edits).chain());
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

struct PendingChunk {
    task: Task<BuiltChunk>,
    /// The chunk's edit version when the task started; a newer edit means remesh again.
    version: u32,
}

/// What the save file holds: the seed plus every edited voxel.
#[derive(Serialize, Deserialize)]
struct SavedEdits {
    seed: u32,
    /// `[x, y, z, block]` in world voxel coordinates.
    edits: Vec<[i32; 4]>,
}

#[derive(Resource)]
pub struct VoxelWorld {
    pub terrain: Terrain,
    palette: Arc<Palette>,
    materials: Vec<Handle<StandardMaterial>>,
    loaded: HashMap<IVec3, LoadedChunk>,
    pending: HashMap<IVec3, PendingChunk>,
    /// Edited voxels, grouped by the chunk that owns them.
    edits: HashMap<IVec3, HashMap<IVec3, VoxelId>>,
    /// Bumped on every edit that touches a chunk (including its border).
    versions: HashMap<IVec3, u32>,
    /// Loaded chunks whose meshes are out of date.
    dirty: HashSet<IVec3>,
    unsaved: bool,
    /// Set while a terrain preview is applied, so it isn't saved until it's kept.
    pub hold_saves: bool,
}

fn chunk_of(p: IVec3) -> IVec3 {
    p.div_euclid(IVec3::splat(CHUNK))
}

impl VoxelWorld {
    pub fn voxel_at(pos: Vec3) -> IVec3 {
        (pos / VOXEL_SIZE).floor().as_ivec3()
    }

    /// The voxel at world voxel coordinate `p`, or `None` if its chunk isn't loaded.
    pub fn get(&self, p: IVec3) -> Option<VoxelId> {
        if p.y >= WORLD_HEIGHT {
            return Some(AIR);
        }
        let chunk = chunk_of(p);
        let chunk = chunk.with_y(chunk.y.clamp(0, VERTICAL_CHUNKS - 1));
        let local = p - chunk * CHUNK + IVec3::ONE;
        self.loaded.get(&chunk).map(|c| c.voxels.get(local))
    }

    /// Fills `out` with world column `(x, z)`, `y = 0..WORLD_HEIGHT`. Returns false
    /// if any of its chunks isn't loaded.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn column(&self, x: i32, z: i32, out: &mut [VoxelId]) -> bool {
        let c = chunk_of(IVec3::new(x, 0, z));
        for cy in 0..VERTICAL_CHUNKS {
            let Some(chunk) = self.loaded.get(&c.with_y(cy)) else {
                return false;
            };
            let local = IVec3::new(x - c.x * CHUNK + 1, 1, z - c.z * CHUNK + 1);
            for y in 0..CHUNK {
                out[(cy * CHUNK + y) as usize] = chunk.voxels.get(local + IVec3::Y * y);
            }
        }
        true
    }

    /// The edit stored at `p`, if any (otherwise it's whatever the seed made).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn edit_at(&self, p: IVec3) -> Option<VoxelId> {
        self.edits.get(&chunk_of(p)).and_then(|c| c.get(&p)).copied()
    }

    /// Changes one voxel. Loaded chunks (and neighbors whose border holds it) update
    /// immediately for collision and remesh in the background. Returns false outside
    /// the editable range (the bottom layer is bedrock).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn set(&mut self, p: IVec3, id: VoxelId) -> bool {
        self.write(p, Some(id), id)
    }

    /// Puts back a voxel as recorded by [`edit_at`](Self::edit_at) and [`get`](Self::get)
    /// before it was changed.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn restore(&mut self, p: IVec3, edit: Option<VoxelId>, visible: VoxelId) {
        self.write(p, edit, visible);
    }

    fn write(&mut self, p: IVec3, edit: Option<VoxelId>, visible: VoxelId) -> bool {
        if p.y < 1 || p.y >= WORLD_HEIGHT {
            return false;
        }
        match edit {
            Some(id) => {
                self.edits.entry(chunk_of(p)).or_default().insert(p, id);
            }
            None => {
                if let Some(chunk) = self.edits.get_mut(&chunk_of(p)) {
                    chunk.remove(&p);
                }
            }
        }
        self.unsaved = true;
        let id = visible;
        // Every chunk whose padded grid contains `p`.
        let lo = chunk_of(p - IVec3::ONE);
        let hi = chunk_of(p + IVec3::ONE);
        for z in lo.z..=hi.z {
            for y in lo.y..=hi.y {
                for x in lo.x..=hi.x {
                    let c = IVec3::new(x, y, z);
                    *self.versions.entry(c).or_default() += 1;
                    if let Some(chunk) = self.loaded.get_mut(&c) {
                        Arc::make_mut(&mut chunk.voxels).set(p - c * CHUNK + IVec3::ONE, id);
                        self.dirty.insert(c);
                    }
                }
            }
        }
        true
    }

    /// Edits inside the padded grid of chunk `c`.
    fn edits_for(&self, c: IVec3) -> Vec<(IVec3, VoxelId)> {
        let (min, max) = (c * CHUNK - IVec3::ONE, c * CHUNK + IVec3::splat(CHUNK));
        let mut out = Vec::new();
        for z in -1..=1 {
            for y in -1..=1 {
                for x in -1..=1 {
                    let Some(edits) = self.edits.get(&(c + IVec3::new(x, y, z))) else {
                        continue;
                    };
                    out.extend(edits.iter().filter(|(p, _)| p.cmpge(min).all() && p.cmple(max).all()).map(|(p, id)| (*p, *id)));
                }
            }
        }
        out
    }

    /// The first solid terrain voxel hit by `ray` within `max` meters. Stops at
    /// unloaded chunks.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn raycast(&self, ray: Ray3d, max: f32) -> Option<Hit> {
        let origin = ray.origin / VOXEL_SIZE;
        let dir = *ray.direction / VOXEL_SIZE;
        let (distance, normal) =
            march(origin, dir, 0.0, max, |cell| self.get(cell).map(terrain::is_solid))?;
        Some(Hit { distance, normal: normal.as_vec3() })
    }

    /// Whether every chunk in the column containing `pos` is loaded.
    pub fn column_loaded(&self, pos: Vec3) -> bool {
        let c = chunk_of(Self::voxel_at(pos));
        (0..VERTICAL_CHUNKS).all(|y| self.loaded.contains_key(&c.with_y(y)))
    }

    fn spawn_task(&mut self, c: IVec3, existing: Option<Arc<VoxelGrid>>) {
        let terrain = self.terrain;
        let palette = self.palette.clone();
        let edits = self.edits_for(c);
        let version = self.versions.get(&c).copied().unwrap_or_default();
        let task = AsyncComputeTaskPool::get().spawn(async move { build_chunk(terrain, &palette, c, existing, &edits) });
        self.pending.insert(c, PendingChunk { task, version });
    }
}

fn setup_world(mut commands: Commands, mut materials: ResMut<Assets<StandardMaterial>>) {
    let palette = terrain::palette();
    let materials = palette
        .surfaces
        .iter()
        .map(|s| materials.add(s.to_material()))
        .collect();
    let mut edits: HashMap<IVec3, HashMap<IVec3, VoxelId>> = default();
    for [x, y, z, id] in load_edits() {
        let p = IVec3::new(x, y, z);
        edits.entry(chunk_of(p)).or_default().insert(p, id as VoxelId);
    }
    commands.insert_resource(VoxelWorld {
        terrain: Terrain { seed: SEED },
        palette: Arc::new(palette),
        materials,
        loaded: default(),
        pending: default(),
        edits,
        versions: default(),
        dirty: default(),
        unsaved: false,
        hold_saves: false,
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn load_edits() -> Vec<[i32; 4]> {
    let Ok(text) = std::fs::read_to_string(EDITS_PATH) else {
        return Vec::new();
    };
    match serde_json::from_str::<SavedEdits>(&text) {
        Ok(saved) if saved.seed == SEED => saved.edits,
        Ok(saved) => {
            warn!("ignoring {EDITS_PATH}: it was made for seed {}, not {SEED}", saved.seed);
            Vec::new()
        }
        Err(e) => {
            error!("couldn't read {EDITS_PATH}: {e}");
            Vec::new()
        }
    }
}

/// The web build has no save file yet; edits last for the session.
#[cfg(target_arch = "wasm32")]
fn load_edits() -> Vec<[i32; 4]> {
    Vec::new()
}

/// Writes edits at most every couple of seconds.
fn save_edits(time: Res<Time>, mut world: ResMut<VoxelWorld>, mut last: Local<f32>) {
    let now = time.elapsed_secs();
    if !world.unsaved || world.hold_saves || now - *last < 2.0 {
        return;
    }
    *last = now;
    world.unsaved = false;
    let edits: Vec<[i32; 4]> = world
        .edits
        .values()
        .flat_map(|chunk| chunk.iter().map(|(p, id)| [p.x, p.y, p.z, *id as i32]))
        .collect();
    let saved = SavedEdits { seed: SEED, edits };
    #[cfg(not(target_arch = "wasm32"))]
    {
        let result = std::fs::create_dir_all("saves")
            .and_then(|_| std::fs::write(EDITS_PATH, serde_json::to_string(&saved).unwrap_or_default()));
        if let Err(e) = result {
            error!("couldn't save {EDITS_PATH}: {e}");
        }
    }
    #[cfg(target_arch = "wasm32")]
    let _ = saved;
}

fn stream_chunks(
    mut commands: Commands,
    mut world: ResMut<VoxelWorld>,
    loaders: Query<&GlobalTransform, With<ChunkLoader>>,
) {
    let centers: Vec<IVec2> = loaders
        .iter()
        .map(|t| {
            let c = chunk_of(VoxelWorld::voxel_at(t.translation()));
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
        world.dirty.remove(&c);
        if let Some(chunk) = world.loaded.remove(&c) {
            for e in chunk.entities {
                commands.entity(e).despawn();
            }
        }
    }

    // Edited chunks remesh first: the player is looking right at them.
    let dirty: Vec<IVec3> = world.dirty.iter().copied().filter(|c| !world.pending.contains_key(c)).collect();
    for c in dirty {
        world.dirty.remove(&c);
        let voxels = world.loaded.get(&c).map(|l| l.voxels.clone());
        if voxels.is_some() {
            world.spawn_task(c, voxels);
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
    for (_, c) in wanted {
        if world.pending.len() >= MAX_IN_FLIGHT {
            break;
        }
        world.spawn_task(c, None);
    }
}

/// Generates a chunk (or reuses `existing` voxels to remesh it), applies edits, and meshes it.
fn build_chunk(
    terrain: Terrain,
    palette: &Palette,
    chunk: IVec3,
    existing: Option<Arc<VoxelGrid>>,
    edits: &[(IVec3, VoxelId)],
) -> BuiltChunk {
    let origin = chunk * CHUNK;
    let padded_min = origin - IVec3::ONE;
    let mut voxels = match existing {
        Some(voxels) => Arc::unwrap_or_clone(voxels),
        None => terrain.generate(padded_min, UVec3::splat((CHUNK + 2) as u32)),
    };
    for (p, id) in edits {
        voxels.set(*p - padded_min, *id);
    }
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
    for (c, pending) in world.pending.iter_mut() {
        if let Some(built) = check_ready(&mut pending.task) {
            done.push((*c, pending.version, built));
        }
    }
    for (c, version, mut built) in done {
        world.pending.remove(&c);
        // Edits made while the task ran: the loaded voxels already have them, so keep
        // those (for collision) and remesh again. A chunk loading for the first time
        // just starts over.
        if world.versions.get(&c).copied().unwrap_or_default() != version {
            match world.loaded.get(&c) {
                Some(loaded) => {
                    built.voxels = (*loaded.voxels).clone();
                    world.dirty.insert(c);
                }
                None => {
                    world.spawn_task(c, None);
                    continue;
                }
            }
        }
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
        // A remesh replaces the old meshes only now, so edits never flash a hole.
        if let Some(old) = world.loaded.insert(c, LoadedChunk { voxels: Arc::new(built.voxels), entities }) {
            for e in old.entities {
                commands.entity(e).despawn();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> VoxelWorld {
        VoxelWorld {
            terrain: Terrain { seed: SEED },
            palette: Arc::new(terrain::palette()),
            materials: Vec::new(),
            loaded: default(),
            pending: default(),
            edits: default(),
            versions: default(),
            dirty: default(),
            unsaved: false,
            hold_saves: false,
        }
    }

    fn load(world: &mut VoxelWorld, c: IVec3) {
        let built = build_chunk(world.terrain, &world.palette.clone(), c, None, &world.edits_for(c));
        world.loaded.insert(c, LoadedChunk { voxels: Arc::new(built.voxels), entities: Vec::new() });
    }

    #[test]
    fn edits_survive_regeneration() {
        let mut world = world();
        let p = IVec3::new(5, 20, 7);
        assert!(world.set(p, block::STONE[0]));
        // Not loaded yet: the edit is only stored.
        assert_eq!(world.get(p), None);
        load(&mut world, IVec3::ZERO);
        assert_eq!(world.get(p), Some(block::STONE[0]));
    }

    #[test]
    fn edits_reach_neighbor_borders_and_mark_them_dirty() {
        let mut world = world();
        let (a, b) = (IVec3::ZERO, IVec3::X);
        load(&mut world, a);
        load(&mut world, b);
        // The last column of chunk `a` is also the border of chunk `b`.
        let p = IVec3::new(CHUNK - 1, 20, 3);
        world.set(p, AIR);
        assert_eq!(world.get(p), Some(AIR));
        let border = world.loaded[&b].voxels.get(p - b * CHUNK + IVec3::ONE);
        assert_eq!(border, AIR);
        assert!(world.dirty.contains(&a) && world.dirty.contains(&b));
        assert!(!world.dirty.contains(&IVec3::NEG_X));
    }

    #[test]
    fn restore_puts_back_the_seed() {
        let mut world = world();
        load(&mut world, IVec3::ZERO);
        let p = IVec3::new(4, 20, 4);
        let (edit, visible) = (world.edit_at(p), world.get(p).unwrap());
        world.set(p, block::SNOW);
        world.restore(p, edit, visible);
        assert_eq!(world.edit_at(p), None);
        assert_eq!(world.get(p), Some(visible));
        let mut column = vec![AIR; WORLD_HEIGHT as usize];
        // Only the bottom chunk is loaded.
        assert!(!world.column(4, 4, &mut column));
    }

    #[test]
    fn bedrock_and_sky_are_not_editable() {
        let mut world = world();
        assert!(!world.set(IVec3::new(0, 0, 0), AIR));
        assert!(!world.set(IVec3::new(0, WORLD_HEIGHT, 0), block::STONE[0]));
    }

    use terrain::block;
}
