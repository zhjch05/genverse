//! An endless, streamed voxel world.
//!
//! Chunks around every [`ChunkLoader`] are generated and meshed on the async compute
//! pool, then spawned as mesh entities. Each loaded chunk keeps its voxels (with a
//! one-voxel border copied from its neighbors) for collision queries.
//!
//! Past the full-detail area the terrain is drawn from coarser and coarser chunks
//! ([`lod`]), which have no voxels to collide with: the hero never gets there before
//! the full detail does.
//!
//! Terrain changes are stored as a sparse layer of voxel edits on top of the seed
//! ([`VoxelWorld::set`]), applied whenever a chunk is (re)built. The edits are what the
//! change log in [`ledger`] replays to, so the world stays "seed + changes".

pub mod ledger;
pub mod lod;

pub use genverse_rules::terrain;

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
use lod::{LEVELS, Node, TOP, VIEW_DISTANCE, build_coarse, coarse_keys, coarse_region, select, span, step};
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
    /// Goes up whenever the edits change, so copies of them know to catch up.
    pub edits_version: u64,
    /// Bumped on every edit that touches a chunk (including its border).
    versions: HashMap<IVec3, u32>,
    /// Loaded chunks whose meshes are out of date.
    dirty: HashSet<IVec3>,
    /// Loaded chunks to regenerate from the seed and edits (after the edits were
    /// replaced wholesale, e.g. by a rollback).
    regen: HashSet<IVec3>,
    /// The nodes to draw: each place in view, at the level it's drawn at.
    wanted: HashSet<Node>,
    /// The nodes split to get them, for the margin a split node keeps.
    split: HashSet<Node>,
    /// Nodes whose chunks are all in and shown. Until then they're hidden, and the
    /// nodes they replace stay.
    shown: HashSet<Node>,
    /// Coarse chunks, by level and chunk coordinates at that level.
    coarse: HashMap<(u32, IVec3), Vec<Entity>>,
    coarse_pending: HashMap<(u32, IVec3), Task<Vec<(usize, Mesh)>>>,
    /// Loaded coarse chunks to rebuild, because edits in their region changed.
    coarse_regen: HashSet<(u32, IVec3)>,
    unsaved: bool,
    /// Set while a terrain preview is applied, so it isn't saved until it's kept.
    pub hold_saves: bool,
}

fn chunk_of(p: IVec3) -> IVec3 {
    p.div_euclid(IVec3::splat(CHUNK))
}

/// The coarse chunks of `node`, a node at level 1 or up.
fn node_coarse(node: Node) -> impl Iterator<Item = (u32, IVec3)> {
    node.chunks().map(move |k| (node.level, k))
}

/// Offline, game pieces walk on the ground as it's loaded here; where it isn't, they go
/// straight.
impl genverse_rules::logic::Ground for VoxelWorld {
    fn heights(&self, min: [i32; 2], step: i32, count: [i32; 2]) -> Option<Vec<Option<i32>>> {
        let mut column = vec![AIR; WORLD_HEIGHT as usize];
        let mut out = Vec::with_capacity((count[0] * count[1]).max(0) as usize);
        for j in 0..count[1] {
            for i in 0..count[0] {
                if !self.column(min[0] + i * step, min[1] + j * step, &mut column) {
                    return None;
                }
                out.push(genverse_rules::logic::walk::stand_in(&column));
            }
        }
        Some(out)
    }
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
        self.edits_version += 1;
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
        for level in 1..LEVELS {
            for key in coarse_keys(level, lo, hi) {
                if self.coarse.contains_key(&(level, key)) {
                    self.coarse_regen.insert((level, key));
                }
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
        self.edits_version += 1;
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

    /// Starts over on other ground (another world): every chunk goes, and they stream in
    /// again from `terrain` with `edits` on top.
    pub fn reset(&mut self, commands: &mut Commands, terrain: Terrain, edits: impl IntoIterator<Item = (IVec3, VoxelId)>) {
        for chunk in self.loaded.drain().map(|(_, c)| c) {
            for e in chunk.entities {
                commands.entity(e).despawn();
            }
        }
        for e in self.coarse.drain().flat_map(|(_, list)| list) {
            commands.entity(e).despawn();
        }
        self.pending.clear();
        self.coarse_pending.clear();
        self.coarse_regen.clear();
        self.dirty.clear();
        self.regen.clear();
        self.wanted.clear();
        self.split.clear();
        self.shown.clear();
        self.versions.clear();
        self.terrain = terrain;
        self.edits = default();
        for (p, id) in edits {
            self.edits.entry(chunk_of(p)).or_default().insert(p, id);
        }
        self.edits_version += 1;
        self.unsaved = false;
        self.hold_saves = false;
    }

    /// Every edit, as `[x, y, z, block]`, for snapshots.
    /// Every edit, by voxel.
    pub fn edit_map(&self) -> std::collections::HashMap<IVec3, VoxelId> {
        self.edits.values().flat_map(|chunk| chunk.iter().map(|(p, id)| (*p, *id))).collect()
    }

    pub fn edit_list(&self) -> Vec<[i32; 4]> {
        self.edits.values().flat_map(|chunk| chunk.iter().map(|(p, id)| [p.x, p.y, p.z, *id as i32])).collect()
    }

    /// The edits in the columns of `area`, as replay keeps them: for working out land
    /// changes against the ground as it is here.
    pub fn edits_in_area(&self, area: genverse_rules::sculpt::Area) -> std::collections::HashMap<IVec3, VoxelId> {
        let min = IVec3::new(area.min[0], 0, area.min[1]);
        let max = IVec3::new(area.min[0] + area.size[0], WORLD_HEIGHT, area.min[1] + area.size[1]);
        self.edits_in(min, max).into_iter().collect()
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
        // The nodes streaming chose, which it keeps while the view moves a little (so a
        // hero moved during loading still finishes); before it has run, the ones it will.
        let chosen;
        let nodes = if self.wanted.is_empty() {
            chosen = select(&[pos.xz()], &default(), |_| false).0;
            &chosen
        } else {
            &self.wanted
        };
        let (mut loaded, mut total) = (0, 0);
        for node in nodes.iter().filter(|n| n.distance(pos.xz()) < VIEW_DISTANCE) {
            let (have, of) = if node.level == 0 {
                count(node.chunks(), |c| self.loaded.contains_key(c))
            } else {
                count(node_coarse(*node), |k| self.coarse.contains_key(k))
            };
            loaded += have;
            total += of;
        }
        (loaded, total)
    }

    pub fn column_loaded(&self, pos: Vec3) -> bool {
        let c = chunk_of(Self::voxel_at(pos));
        (0..VERTICAL_CHUNKS).all(|y| self.loaded.contains_key(&c.with_y(y)))
    }

    /// Whether every chunk of `node` is in.
    fn node_loaded(&self, node: Node) -> bool {
        if node.level == 0 {
            node.chunks().all(|c| self.loaded.contains_key(&c))
        } else {
            node_coarse(node).all(|k| self.coarse.contains_key(&k))
        }
    }

    /// Whether any chunk of `node` is in, to be drawn.
    fn node_built(&self, node: Node) -> bool {
        if node.level == 0 {
            node.chunks().any(|c| self.loaded.contains_key(&c))
        } else {
            node_coarse(node).any(|k| self.coarse.contains_key(&k))
        }
    }

    fn node_entities(&self, node: Node) -> Vec<Entity> {
        if node.level == 0 {
            node.chunks().filter_map(|c| self.loaded.get(&c)).flat_map(|l| l.entities.iter().copied()).collect()
        } else {
            node_coarse(node).filter_map(|k| self.coarse.get(&k)).flatten().copied().collect()
        }
    }

    /// Whether the wanted nodes over `node` (one that isn't wanted) are all in, so it
    /// can go: an ancestor of it, or the descendants that tile it. True when there are
    /// none, out of view.
    fn covered(&self, node: Node) -> bool {
        let mut up = node;
        while up.level < TOP {
            up = up.parent();
            if self.wanted.contains(&up) {
                return self.node_loaded(up);
            }
        }
        self.descendants_loaded(node)
    }

    fn descendants_loaded(&self, node: Node) -> bool {
        node.level == 0
            || node.children().iter().all(|c| {
                if self.wanted.contains(c) { self.node_loaded(*c) } else { self.descendants_loaded(*c) }
            })
    }

    fn spawn_coarse_task(&mut self, level: u32, key: IVec3) {
        let terrain = self.terrain;
        let palette = self.palette.clone();
        let (min, max) = coarse_region(level, key);
        let edits = self.edits_in(min, max);
        let task = AsyncComputeTaskPool::get().spawn(async move { build_coarse(terrain, &palette, level, key, &edits) });
        self.coarse_pending.insert((level, key), task);
    }

    /// Stops building whatever of `node` is still building.
    fn cancel(&mut self, node: Node) {
        if node.level == 0 {
            for c in node.chunks() {
                self.pending.remove(&c);
            }
        } else {
            for k in node_coarse(node) {
                self.coarse_pending.remove(&k);
            }
        }
    }

    /// Unloads a node's chunks, and cancels any still building.
    fn drop_node(&mut self, commands: &mut Commands, node: Node) {
        self.cancel(node);
        if node.level == 0 {
            for c in node.chunks() {
                self.dirty.remove(&c);
                self.regen.remove(&c);
                if let Some(chunk) = self.loaded.remove(&c) {
                    for e in chunk.entities {
                        commands.entity(e).despawn();
                    }
                }
            }
        } else {
            for k in node_coarse(node) {
                self.coarse_regen.remove(&k);
                for e in self.coarse.remove(&k).unwrap_or_default() {
                    commands.entity(e).despawn();
                }
            }
        }
        self.shown.remove(&node);
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
    let terrain = ledger.log.terrain();
    commands.insert_resource(ledger);
    commands.insert_resource(VoxelWorld {
        terrain,
        palette: Arc::new(palette),
        materials,
        loaded: default(),
        pending: default(),
        edits,
        edits_version: 0,
        versions: default(),
        dirty: default(),
        regen: default(),
        wanted: default(),
        split: default(),
        shown: default(),
        coarse: default(),
        coarse_pending: default(),
        coarse_regen: default(),
        unsaved: false,
        hold_saves: false,
    });
}

/// Writes the snapshot (voxels and games) at most every couple of seconds when the
/// voxels changed, and every half minute when only the games did (a busy game's timers
/// change them all the time; the log has every change anyway). Never while a preview is
/// applied.
fn save_edits(
    time: Res<Time>,
    ledger: Res<Ledger>,
    mut world: ResMut<VoxelWorld>,
    mut last: Local<(f32, u64)>,
) {
    let now = time.elapsed_secs();
    let wait = if world.unsaved { 2.0 } else { 30.0 };
    // Online, the world lives on the server; the local snapshot belongs to the local world.
    if ledger.is_online() || !(world.unsaved || ledger.version != last.1) || world.hold_saves || now - last.0 < wait {
        return;
    }
    *last = (now, ledger.version);
    world.unsaved = false;
    ledger::write_snapshot(&ledger, world.edit_list());
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
    let points: Vec<Vec2> = loaders.iter().map(|t| t.translation().xz()).collect();
    let (wanted, split) = select(&points, &world.split, |n| world.node_built(n));
    (world.wanted, world.split) = (wanted, split);

    // Swap levels once the new nodes are all in, so moving never opens a hole: an old
    // node goes once what replaces it is in, and a new one shows once nothing old
    // overlaps it, in the same frame.
    let mut present: HashSet<Node> = world.loaded.keys().chain(world.pending.keys()).map(|c| Node::of_column(c.xz())).collect();
    present.extend(world.coarse.keys().chain(world.coarse_pending.keys()).map(|(level, k)| Node::new(*level, k.xz())));
    let mut stale = Vec::new();
    for node in present {
        if world.wanted.contains(&node) {
            continue;
        }
        world.cancel(node);
        if !world.node_built(node) || world.covered(node) {
            world.drop_node(&mut commands, node);
        } else {
            stale.push(node);
        }
    }
    let ready: Vec<Node> = world
        .wanted
        .iter()
        .filter(|n| !world.shown.contains(*n) && world.node_loaded(**n) && !stale.iter().any(|s| s.overlaps(**n)))
        .copied()
        .collect();
    for node in ready {
        for e in world.node_entities(node) {
            commands.entity(e).insert(Visibility::Inherited);
        }
        world.shown.insert(node);
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
    let coarse_regen: Vec<(u32, IVec3)> = world.coarse_regen.iter().copied().filter(|k| !world.coarse_pending.contains_key(k)).collect();
    for (level, key) in coarse_regen {
        world.coarse_regen.remove(&(level, key));
        world.spawn_coarse_task(level, key);
    }

    // Queue the nearest missing chunks, finer before coarser.
    let distance = |node: Node| points.iter().map(|p| node.distance(*p)).fold(f32::INFINITY, f32::min);
    let mut wanted_chunks = Vec::new();
    for node in &world.wanted {
        let d = distance(*node);
        if node.level == 0 {
            wanted_chunks.extend(
                node.chunks().filter(|c| !world.loaded.contains_key(c) && !world.pending.contains_key(c)).map(|c| (d, 0, c)),
            );
        } else {
            wanted_chunks.extend(
                node_coarse(*node)
                    .filter(|k| !world.coarse.contains_key(k) && !world.coarse_pending.contains_key(k))
                    .map(|(level, k)| (d, level, k)),
            );
        }
    }
    wanted_chunks.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    for (_, level, c) in wanted_chunks {
        if world.pending.len() + world.coarse_pending.len() >= MAX_IN_FLIGHT {
            break;
        }
        if level == 0 {
            world.spawn_task(c, None);
        } else {
            world.spawn_coarse_task(level, c);
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
        let visibility = if world.shown.contains(&Node::of_column(c.xz())) { Visibility::Inherited } else { Visibility::Hidden };
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
    for ((level, key), built) in done {
        world.coarse_pending.remove(&(level, key));
        let translation = (key * span(level)).as_vec3() * VOXEL_SIZE;
        let node = Node::new(level, key.xz());
        let visibility = if world.shown.contains(&node) { Visibility::Inherited } else { Visibility::Hidden };
        let entities = built
            .into_iter()
            .map(|(surface, mesh)| {
                let mut e = commands.spawn((
                    WorldChunk,
                    Mesh3d(meshes.add(mesh)),
                    MeshMaterial3d(world.materials[surface].clone()),
                    Transform::from_translation(translation).with_scale(Vec3::splat(step(level) as f32 * VOXEL_SIZE)),
                    visibility,
                ));
                if world.palette.surfaces[surface].is_transparent() {
                    e.insert(NotShadowCaster);
                }
                e.id()
            })
            .collect();
        if let Some(old) = world.coarse.insert((level, key), entities) {
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
            terrain: Terrain { seed: SEED, flat: false },
            palette: Arc::new(terrain::palette()),
            materials: vec![Handle::default(); terrain::palette().surfaces.len()],
            loaded: default(),
            pending: default(),
            edits: default(),
            edits_version: 0,
            versions: default(),
            dirty: default(),
            regen: default(),
            wanted: default(),
            split: default(),
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

    /// How many shown nodes cover each point of an 8 m grid (the points at the middle of
    /// level-0 nodes, never on an edge).
    fn coverage(world: &VoxelWorld) -> HashMap<IVec2, u32> {
        let mut out: HashMap<IVec2, u32> = default();
        for node in &world.shown {
            let n = 1 << node.level;
            for dz in 0..n {
                for dx in 0..n {
                    *out.entry(node.xz * n + IVec2::new(dx, dz)).or_default() += 1;
                }
            }
        }
        out
    }

    /// Every point within `radius` of all of `from` is drawn once: by one shown node.
    fn check_drawn_once(world: &VoxelWorld, from: &[Vec2], radius: f32) {
        let coverage = coverage(world);
        let reach = (radius / 8.0) as i32 + 1;
        for z in -reach..=reach {
            for x in -reach..=reach {
                let column = (from[0] / 8.0).floor().as_ivec2() + IVec2::new(x, z);
                let p = column.as_vec2() * 8.0 + 4.0;
                if from.iter().any(|f| f.distance(p) > radius) {
                    continue;
                }
                let n = coverage.get(&column).copied().unwrap_or(0);
                assert_eq!(n, 1, "column {column} is drawn {n} times");
            }
        }
    }

    fn run_until_loaded(app: &mut App, pos: Vec3) {
        for _ in 0..50_000 {
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
        // One more frame swaps in the last nodes.
        app.update();
        let inner = VIEW_DISTANCE * 0.9;
        check_drawn_once(app.world().resource::<VoxelWorld>(), &[Vec2::ZERO], inner);
        let levels: HashSet<u32> = app.world().resource::<VoxelWorld>().shown.iter().map(|n| n.level).collect();
        assert_eq!(levels.len(), LEVELS as usize);

        // Walk 48 m east: nodes change level, and every place stays drawn once on
        // every frame while they do.
        let to = Vec2::new(48.0, 0.0);
        app.world_mut().entity_mut(loader).insert(GlobalTransform::from_translation(to.extend(0.0).xzy()));
        for _ in 0..50_000 {
            app.update();
            let world = app.world().resource::<VoxelWorld>();
            check_drawn_once(world, &[Vec2::ZERO, to], inner - 48.0);
            let (loaded, total) = world.view_loaded(to.extend(0.0).xzy());
            if loaded == total && world.pending.is_empty() && world.coarse_pending.is_empty() {
                break;
            }
            std::thread::yield_now();
        }
        app.update();
        check_drawn_once(app.world().resource::<VoxelWorld>(), &[to], inner);
    }
}

#[cfg(test)]
mod mesh_stats {
    use super::*;

    fn quads(meshes: &[(usize, Mesh)]) -> usize {
        meshes.iter().map(|(_, m)| m.count_vertices() / 4).sum()
    }

    /// Prints the quads drawn at each level around a loader, and how long they took.
    #[test]
    #[ignore]
    fn print_mesh_stats() {
        let terrain = Terrain { seed: SEED, flat: false };
        let palette = terrain::palette();
        let (nodes, _) = select(&[Vec2::new(4.0, 4.0)], &default(), |_| false);
        let mut per_level = [(0usize, 0usize); LEVELS as usize];
        let t = std::time::Instant::now();
        for node in nodes {
            let entry = &mut per_level[node.level as usize];
            entry.0 += 1;
            for c in node.chunks() {
                entry.1 += if node.level == 0 {
                    quads(&build_chunk(terrain, &palette, c, None, &[]).meshes)
                } else {
                    quads(&build_coarse(terrain, &palette, node.level, c, &[]))
                };
            }
        }
        for (level, (n, q)) in per_level.iter().enumerate() {
            println!("level {level}: {n} nodes, {q} quads ({} per node)", q / n.max(&1));
        }
        let total: usize = per_level.iter().map(|l| l.1).sum();
        println!("total {total} quads in {:?}, view {VIEW_DISTANCE} m", t.elapsed());
    }
}
