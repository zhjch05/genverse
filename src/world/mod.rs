//! An endless, streamed voxel world.
//!
//! Chunks around every [`ChunkLoader`] are generated and meshed on the async compute
//! pool, then spawned as mesh entities. Each loaded chunk keeps its voxels (with a
//! one-voxel border copied from its neighbors) for collision queries.
//!
//! Past the full-detail area, out to twice as far, the terrain is drawn from coarse
//! chunks ([`lod`]), which have no voxels to collide with: the hero never gets there
//! before the full detail does.
//!
//! Terrain changes are stored as a sparse layer of voxel edits on top of the seed
//! ([`VoxelWorld::set`]), applied whenever a chunk is (re)built. The edits are what the
//! change log in [`ledger`] replays to, so the world stays "seed + changes".

pub mod ledger;
pub mod lod;

pub use cubewish_rules::terrain;

use std::sync::Arc;

use bevy::{
    light::NotShadowCaster,
    platform::collections::{HashMap, HashSet},
    prelude::*,
    tasks::{AsyncComputeTaskPool, Task, futures::check_ready},
};

use crate::voxel::{
    AIR, Palette, SurfaceMaterial, VoxelGrid, VoxelId, VoxelMaterial,
    mesher::mesh_region,
    query::{Hit, march},
};
use ledger::Ledger;
use lod::{CELL, COARSE, COARSE_SPAN, COARSE_VERTICAL, Lod, SCAN, build_coarse, cell_of_column, coarse_region, wanted};
use terrain::{Terrain, VOXEL_SIZE, WORLD_HEIGHT};

pub const CHUNK: i32 = 32;
const VERTICAL_CHUNKS: i32 = WORLD_HEIGHT / CHUNK;
const MAX_IN_FLIGHT: usize = if cfg!(target_arch = "wasm32") { 2 } else { 12 };
pub const SEED: u32 = 1337;

pub struct WorldPlugin;

impl Plugin for WorldPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PreStartup, setup_world)
            .add_systems(Update, ((stream_chunks, finish_chunks, save_edits).chain(), apply_look));
    }
}

/// Keeps every voxel material, including ones made since, drawn the way the world's
/// rules say.
fn apply_look(ledger: Res<Ledger>, mut materials: ResMut<Assets<VoxelMaterial>>) {
    let bevel = ledger.rules.look.bevel;
    let stale: Vec<_> = materials.iter().filter(|(_, m)| m.extension.width() != bevel).map(|(id, _)| id).collect();
    for id in stale {
        if let Some(mut material) = materials.get_mut(id) {
            material.extension.set_width(bevel);
        }
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

#[derive(Resource)]
pub struct VoxelWorld {
    pub terrain: Terrain,
    palette: Arc<Palette>,
    materials: Vec<Handle<VoxelMaterial>>,
    loaded: HashMap<IVec3, LoadedChunk>,
    pending: HashMap<IVec3, PendingChunk>,
    /// Edited voxels, grouped by the chunk that owns them.
    edits: HashMap<IVec3, HashMap<IVec3, VoxelId>>,
    /// Bumped on every edit that touches a chunk (including its border).
    versions: HashMap<IVec3, u32>,
    /// Loaded chunks whose meshes are out of date.
    dirty: HashSet<IVec3>,
    /// Loaded chunks to regenerate from the seed and edits (after the edits were
    /// replaced wholesale, e.g. by a rollback).
    regen: HashSet<IVec3>,
    /// The level each cell in view is drawn at.
    cells: HashMap<IVec2, Lod>,
    /// Cells whose full-detail chunks are all in and shown. Until then they're hidden,
    /// and the coarse chunk (if any) stays.
    shown: HashSet<IVec2>,
    coarse: HashMap<IVec3, Vec<Entity>>,
    coarse_pending: HashMap<IVec3, Task<Vec<(usize, Mesh)>>>,
    /// Loaded coarse chunks to rebuild, because edits in their region changed.
    coarse_regen: HashSet<IVec3>,
    unsaved: bool,
    /// Set while a terrain preview is applied, so it isn't saved until it's kept.
    pub hold_saves: bool,
}

fn chunk_of(p: IVec3) -> IVec3 {
    p.div_euclid(IVec3::splat(CHUNK))
}

/// The chunk column under `pos`.
fn column_of(pos: Vec3) -> IVec2 {
    chunk_of(VoxelWorld::voxel_at(pos)).xz()
}

/// The full-detail chunks of a cell.
fn cell_chunks(cell: IVec2) -> impl Iterator<Item = IVec3> {
    let corner = cell * CELL;
    (0..CELL).flat_map(move |dz| {
        (0..CELL).flat_map(move |dx| (0..VERTICAL_CHUNKS).map(move |y| IVec3::new(corner.x + dx, y, corner.y + dz)))
    })
}

/// The coarse chunks of a cell, keyed by coarse chunk coordinates.
fn cell_coarse(cell: IVec2) -> impl Iterator<Item = IVec3> {
    (0..COARSE_VERTICAL).map(move |y| IVec3::new(cell.x, y, cell.y))
}

/// The coarse chunks whose region holds any voxel in `lo..=hi`.
fn coarse_keys(lo: IVec3, hi: IVec3) -> impl Iterator<Item = IVec3> {
    let span = IVec3::splat(COARSE_SPAN);
    let (lo, hi) = ((lo - IVec3::splat(COARSE)).div_euclid(span), (hi + IVec3::splat(COARSE)).div_euclid(span));
    (lo.z..=hi.z).flat_map(move |z| (lo.y..=hi.y).flat_map(move |y| (lo.x..=hi.x).map(move |x| IVec3::new(x, y, z))))
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
        self.mark_coarse(p, p);
        true
    }

    /// Queues a rebuild of the loaded coarse chunks whose region holds any of `lo..=hi`.
    fn mark_coarse(&mut self, lo: IVec3, hi: IVec3) {
        for key in coarse_keys(lo, hi) {
            if self.coarse.contains_key(&key) {
                self.coarse_regen.insert(key);
            }
        }
    }

    /// Replaces every edit with `edits` (the result of replaying the change log), and
    /// regenerates the loaded chunks that differ. For changes that aren't a simple
    /// stack of undos: rollbacks, restores, and later other players' changes.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn replace_edits(&mut self, edits: impl IntoIterator<Item = (IVec3, VoxelId)>) {
        let mut next: HashMap<IVec3, HashMap<IVec3, VoxelId>> = default();
        for (p, id) in edits {
            next.entry(chunk_of(p)).or_default().insert(p, id);
        }
        let same = |a: &HashMap<IVec3, HashMap<IVec3, VoxelId>>, b: &HashMap<IVec3, HashMap<IVec3, VoxelId>>, c: IVec3| {
            a.get(&c).filter(|m| !m.is_empty()) == b.get(&c).filter(|m| !m.is_empty())
        };
        let owners: HashSet<IVec3> = self.edits.keys().chain(next.keys()).copied().collect();
        let changed: Vec<IVec3> = owners.into_iter().filter(|c| !same(&self.edits, &next, *c)).collect();
        self.edits = next;
        // A chunk's padded grid also holds its neighbors' border voxels.
        for owner in changed {
            for z in -1..=1 {
                for y in -1..=1 {
                    for x in -1..=1 {
                        let c = owner + IVec3::new(x, y, z);
                        *self.versions.entry(c).or_default() += 1;
                        if self.loaded.contains_key(&c) {
                            self.regen.insert(c);
                        }
                    }
                }
            }
            self.mark_coarse(owner * CHUNK, owner * CHUNK + IVec3::splat(CHUNK - 1));
        }
        self.unsaved = true;
    }

    /// Edits inside the padded grid of chunk `c`.
    fn edits_for(&self, c: IVec3) -> Vec<(IVec3, VoxelId)> {
        self.edits_in(c * CHUNK - IVec3::ONE, c * CHUNK + IVec3::splat(CHUNK + 1))
    }

    /// Edits in `min..max`.
    fn edits_in(&self, min: IVec3, max: IVec3) -> Vec<(IVec3, VoxelId)> {
        let (lo, hi) = (chunk_of(min), chunk_of(max - IVec3::ONE));
        let mut out = Vec::new();
        for z in lo.z..=hi.z {
            for y in lo.y..=hi.y {
                for x in lo.x..=hi.x {
                    let Some(edits) = self.edits.get(&IVec3::new(x, y, z)) else {
                        continue;
                    };
                    out.extend(edits.iter().filter(|(p, _)| p.cmpge(min).all() && p.cmplt(max).all()).map(|(p, id)| (*p, *id)));
                }
            }
        }
        out
    }

    /// The first solid terrain voxel hit by `ray` within `max` meters. Stops at
    /// unloaded chunks.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn raycast(&self, ray: Ray3d, max: f32) -> Option<Hit> {
        self.raycast_where(ray, max, terrain::is_solid)
    }

    /// Like [`Self::raycast`], but sees only the ground and water surface: trees,
    /// tufts and flowers let the ray through, so selecting land isn't caught on them.
    pub fn raycast_ground(&self, ray: Ray3d, max: f32) -> Option<Hit> {
        self.raycast_where(ray, max, |id| terrain::is_ground(id) || id == terrain::block::WATER)
    }

    fn raycast_where(&self, ray: Ray3d, max: f32, hits: impl Fn(VoxelId) -> bool) -> Option<Hit> {
        let origin = ray.origin / VOXEL_SIZE;
        let dir = *ray.direction / VOXEL_SIZE;
        let (distance, normal) = march(origin, dir, 0.0, max, |cell| self.get(cell).map(&hits))?;
        Some(Hit { distance, normal: normal.as_vec3() })
    }

    /// Whether every chunk in the column containing `pos` is loaded.
    /// How many chunks are loaded, and how many are still being built.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn chunk_counts(&self) -> (usize, usize) {
        (self.loaded.len(), self.pending.len())
    }

    /// How much of the terrain in view of `pos` is in: chunks loaded, out of how many
    /// there are, full-detail ones near and coarse ones farther out.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn view_loaded(&self, pos: Vec3) -> (usize, usize) {
        let center = cell_of_column(column_of(pos));
        let (mut loaded, mut total) = (0, 0);
        for dz in -SCAN..=SCAN {
            for dx in -SCAN..=SCAN {
                let cell = center + IVec2::new(dx, dz);
                let (have, of) = match wanted(dx * dx + dz * dz, None) {
                    Some(Lod::Near) => count(cell_chunks(cell), |c| self.loaded.contains_key(c)),
                    Some(Lod::Far) => count(cell_coarse(cell), |k| self.coarse.contains_key(k)),
                    None => (0, 0),
                };
                loaded += have;
                total += of;
            }
        }
        (loaded, total)
    }

    pub fn column_loaded(&self, pos: Vec3) -> bool {
        let c = chunk_of(Self::voxel_at(pos));
        (0..VERTICAL_CHUNKS).all(|y| self.loaded.contains_key(&c.with_y(y)))
    }

    fn spawn_coarse_task(&mut self, key: IVec3) {
        let terrain = self.terrain;
        let palette = self.palette.clone();
        let (min, max) = coarse_region(key);
        let edits = self.edits_in(min, max);
        let task = AsyncComputeTaskPool::get().spawn(async move { build_coarse(terrain, &palette, key, &edits) });
        self.coarse_pending.insert(key, task);
    }

    /// Unloads a cell's full-detail chunks, and cancels any still building.
    fn drop_near(&mut self, commands: &mut Commands, cell: IVec2) {
        for c in cell_chunks(cell) {
            self.dirty.remove(&c);
            self.regen.remove(&c);
            self.pending.remove(&c);
            if let Some(chunk) = self.loaded.remove(&c) {
                for e in chunk.entities {
                    commands.entity(e).despawn();
                }
            }
        }
        self.shown.remove(&cell);
    }

    /// Unloads a cell's coarse chunks, and cancels any still building.
    fn drop_coarse(&mut self, commands: &mut Commands, cell: IVec2) {
        for key in cell_coarse(cell) {
            self.coarse_regen.remove(&key);
            self.coarse_pending.remove(&key);
            for e in self.coarse.remove(&key).unwrap_or_default() {
                commands.entity(e).despawn();
            }
        }
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

fn setup_world(mut commands: Commands, mut materials: ResMut<Assets<VoxelMaterial>>) {
    let palette = terrain::palette();
    let materials = palette
        .surfaces
        .iter()
        .map(|s| materials.add(s.to_material()))
        .collect();
    let (ledger, loaded) = ledger::load(SEED);
    let mut edits: HashMap<IVec3, HashMap<IVec3, VoxelId>> = default();
    for (p, id) in loaded {
        edits.entry(chunk_of(p)).or_default().insert(p, id);
    }
    commands.insert_resource(ledger);
    commands.insert_resource(VoxelWorld {
        terrain: Terrain { seed: SEED },
        palette: Arc::new(palette),
        materials,
        loaded: default(),
        pending: default(),
        edits,
        versions: default(),
        dirty: default(),
        regen: default(),
        cells: default(),
        shown: default(),
        coarse: default(),
        coarse_pending: default(),
        coarse_regen: default(),
        unsaved: false,
        hold_saves: false,
    });
}

/// Writes the voxel cache at most every couple of seconds, when the voxels or the log
/// changed (never while a preview is applied).
fn save_edits(
    time: Res<Time>,
    ledger: Res<Ledger>,
    mut world: ResMut<VoxelWorld>,
    mut last: Local<(f32, u64)>,
) {
    let now = time.elapsed_secs();
    // Online, the world lives on the server; the local cache belongs to the local world.
    if ledger.is_online() || !(world.unsaved || ledger.version != last.1) || world.hold_saves || now - last.0 < 2.0 {
        return;
    }
    *last = (now, ledger.version);
    world.unsaved = false;
    let edits: Vec<[i32; 4]> = world
        .edits
        .values()
        .flat_map(|chunk| chunk.iter().map(|(p, id)| [p.x, p.y, p.z, *id as i32]))
        .collect();
    ledger::write_cache(&ledger.log, edits);
}

/// How many of `items` pass `have`, and how many there are.
fn count<T>(items: impl Iterator<Item = T>, have: impl Fn(&T) -> bool) -> (usize, usize) {
    items.fold((0, 0), |(n, of), item| (n + have(&item) as usize, of + 1))
}

fn stream_chunks(
    mut commands: Commands,
    mut world: ResMut<VoxelWorld>,
    loaders: Query<&GlobalTransform, With<ChunkLoader>>,
) {
    let world = &mut *world;
    let centers: Vec<IVec2> = loaders.iter().map(|t| cell_of_column(column_of(t.translation()))).collect();
    let distance = |cell: IVec2| centers.iter().map(|c| (cell - *c).length_squared()).min().unwrap_or(i32::MAX);

    // Each cell's level, from its nearest loader.
    let mut cells: HashMap<IVec2, Lod> = default();
    for center in &centers {
        for dz in -SCAN..=SCAN {
            for dx in -SCAN..=SCAN {
                let cell = *center + IVec2::new(dx, dz);
                if !cells.contains_key(&cell)
                    && let Some(lod) = wanted(distance(cell), world.cells.get(&cell).copied())
                {
                    cells.insert(cell, lod);
                }
            }
        }
    }
    world.cells = cells;

    // Swap levels once the new one is all in, so moving never opens a hole.
    let mut touched: HashSet<IVec2> = world.cells.keys().copied().collect();
    touched.extend(world.loaded.keys().map(|c| cell_of_column(c.xz())));
    touched.extend(world.coarse.keys().map(|k| k.xz()));
    for cell in touched {
        match world.cells.get(&cell) {
            Some(Lod::Near) => {
                if cell_chunks(cell).all(|c| world.loaded.contains_key(&c)) {
                    if world.shown.insert(cell) {
                        for c in cell_chunks(cell) {
                            for e in &world.loaded[&c].entities {
                                commands.entity(*e).insert(Visibility::Inherited);
                            }
                        }
                    }
                    world.drop_coarse(&mut commands, cell);
                }
            }
            Some(Lod::Far) => {
                for c in cell_chunks(cell) {
                    world.pending.remove(&c);
                }
                if cell_coarse(cell).all(|k| world.coarse.contains_key(&k)) {
                    world.drop_near(&mut commands, cell);
                }
            }
            None => {
                world.drop_near(&mut commands, cell);
                world.drop_coarse(&mut commands, cell);
            }
        }
    }

    // Regenerated chunks first, then edited ones: the player is looking right at them.
    let regen: Vec<IVec3> = world.regen.iter().copied().filter(|c| !world.pending.contains_key(c)).collect();
    for c in regen {
        world.regen.remove(&c);
        world.dirty.remove(&c);
        world.spawn_task(c, None);
    }
    let dirty: Vec<IVec3> = world.dirty.iter().copied().filter(|c| !world.pending.contains_key(c)).collect();
    for c in dirty {
        world.dirty.remove(&c);
        let voxels = world.loaded.get(&c).map(|l| l.voxels.clone());
        if voxels.is_some() {
            world.spawn_task(c, voxels);
        }
    }
    let coarse_regen: Vec<IVec3> = world.coarse_regen.iter().copied().filter(|k| !world.coarse_pending.contains_key(k)).collect();
    for key in coarse_regen {
        world.coarse_regen.remove(&key);
        world.spawn_coarse_task(key);
    }

    // Queue the nearest missing chunks, full detail before coarse.
    let mut wanted_chunks = Vec::new();
    for (cell, lod) in &world.cells {
        let d = distance(*cell);
        match lod {
            Lod::Near => wanted_chunks.extend(
                cell_chunks(*cell)
                    .filter(|c| !world.loaded.contains_key(c) && !world.pending.contains_key(c))
                    .map(|c| (d, false, c)),
            ),
            Lod::Far => wanted_chunks.extend(
                cell_coarse(*cell)
                    .filter(|k| !world.coarse.contains_key(k) && !world.coarse_pending.contains_key(k))
                    .map(|k| (d, true, k)),
            ),
        }
    }
    wanted_chunks.sort_by_key(|(d, coarse, _)| (*d, *coarse));
    for (_, coarse, c) in wanted_chunks {
        if world.pending.len() + world.coarse_pending.len() >= MAX_IN_FLIGHT {
            break;
        }
        if coarse {
            world.spawn_coarse_task(c);
        } else {
            world.spawn_task(c, None);
        }
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
    let meshes = mesh_region(|p| voxels.get(p - padded_min), palette, origin, origin + IVec3::splat(CHUNK));
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
            // Its edits were replaced while it built; a fresh build is queued.
            if world.regen.contains(&c) {
                continue;
            }
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
        // Hidden until its whole cell is in; see `stream_chunks`.
        let visibility = if world.shown.contains(&cell_of_column(c.xz())) { Visibility::Inherited } else { Visibility::Hidden };
        let entities = built
            .meshes
            .into_iter()
            .map(|(surface, mesh)| {
                let mut e = commands.spawn((
                    WorldChunk,
                    Mesh3d(meshes.add(mesh)),
                    MeshMaterial3d(world.materials[surface].clone()),
                    Transform::from_translation(translation).with_scale(Vec3::splat(VOXEL_SIZE)),
                    visibility,
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

    let mut done = Vec::new();
    for (key, task) in world.coarse_pending.iter_mut() {
        if let Some(built) = check_ready(task) {
            done.push((*key, built));
        }
    }
    for (key, built) in done {
        world.coarse_pending.remove(&key);
        let translation = (key * COARSE_SPAN).as_vec3() * VOXEL_SIZE;
        let entities = built
            .into_iter()
            .map(|(surface, mesh)| {
                let mut e = commands.spawn((
                    WorldChunk,
                    Mesh3d(meshes.add(mesh)),
                    MeshMaterial3d(world.materials[surface].clone()),
                    Transform::from_translation(translation).with_scale(Vec3::splat(COARSE as f32 * VOXEL_SIZE)),
                ));
                if world.palette.surfaces[surface].is_transparent() {
                    e.insert(NotShadowCaster);
                }
                e.id()
            })
            .collect();
        if let Some(old) = world.coarse.insert(key, entities) {
            for e in old {
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
            materials: vec![Handle::default(); terrain::palette().surfaces.len()],
            loaded: default(),
            pending: default(),
            edits: default(),
            versions: default(),
            dirty: default(),
            regen: default(),
            cells: default(),
            shown: default(),
            coarse: default(),
            coarse_pending: default(),
            coarse_regen: default(),
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

    fn streaming_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .init_resource::<Assets<Mesh>>()
            .insert_resource(world())
            .add_systems(Update, (stream_chunks, finish_chunks).chain());
        app
    }

    /// Every cell in view is drawn at exactly one level: its full-detail chunks are
    /// shown, or its coarse chunks are loaded, never both.
    fn check_cells(world: &VoxelWorld, center: IVec2) {
        for dz in -SCAN..=SCAN {
            for dx in -SCAN..=SCAN {
                if wanted(dx * dx + dz * dz, None).is_none() {
                    continue;
                }
                let cell = center + IVec2::new(dx, dz);
                let near = world.shown.contains(&cell);
                let coarse = cell_coarse(cell).all(|k| world.coarse.contains_key(&k));
                assert!(near != coarse, "cell {cell}: shown {near}, coarse {coarse}");
            }
        }
    }

    fn run_until_loaded(app: &mut App, pos: Vec3) {
        for _ in 0..20_000 {
            app.update();
            let (loaded, total) = app.world().resource::<VoxelWorld>().view_loaded(pos);
            if loaded == total {
                return;
            }
            std::thread::yield_now();
        }
        panic!("the view never loaded");
    }

    #[test]
    fn levels_never_overlap_or_leave_holes() {
        let mut app = streaming_app();
        let loader = app.world_mut().spawn((ChunkLoader, GlobalTransform::default())).id();
        run_until_loaded(&mut app, Vec3::ZERO);
        // One more frame swaps in the last cells.
        app.update();
        check_cells(app.world().resource::<VoxelWorld>(), IVec2::ZERO);

        // Walk three cells east: cells change level, and every cell stays drawn on
        // every frame while they do.
        let cell_meters = (CELL * CHUNK) as f32 * VOXEL_SIZE;
        let pos = Vec3::new(3.0 * cell_meters, 0.0, 0.0);
        app.world_mut().entity_mut(loader).insert(GlobalTransform::from_translation(pos));
        for _ in 0..20_000 {
            app.update();
            let world = app.world().resource::<VoxelWorld>();
            for dz in -SCAN..=SCAN {
                for dx in -SCAN..=SCAN + 3 {
                    let cell = IVec2::new(dx, dz);
                    // Only cells in view from both places.
                    let in_view = |from: IVec2| wanted((cell - from).length_squared(), None).is_some();
                    if !in_view(IVec2::ZERO) || !in_view(IVec2::new(3, 0)) {
                        continue;
                    }
                    let drawn = world.shown.contains(&cell) || cell_coarse(cell).all(|k| world.coarse.contains_key(&k));
                    assert!(drawn, "cell {cell} has a hole");
                }
            }
            let (loaded, total) = world.view_loaded(pos);
            if loaded == total && world.pending.is_empty() && world.coarse_pending.is_empty() {
                break;
            }
            std::thread::yield_now();
        }
        app.update();
        check_cells(app.world().resource::<VoxelWorld>(), IVec2::new(3, 0));
    }
}
