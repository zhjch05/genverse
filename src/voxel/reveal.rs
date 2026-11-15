//! Voxels that arrive one after another, the way an answer's words do.
//!
//! A change is whole the moment it's made: the grid, collision and the log all have
//! it. Only the drawing takes its time. The mesh of the new state carries, per voxel,
//! when it arrives ([`Fresh`], an order from 1 to 255), and `voxel_vertex.wesl` grows
//! each voxel from nothing as the reveal's clock passes its order. What the change took
//! away is drawn by a second mesh, a ghost, whose voxels shrink away in the same order.
//! When the reveal is over a plain mesh replaces both.
//!
//! A reveal is a slot in one small buffer every voxel material reads
//! ([`REVEAL_BUFFER`]): when it started and how long it takes. The meshes taking part
//! name their slot in their [`MeshTag`], so materials stay shared and nothing is
//! re-specialized; a frame of a reveal uploads a kilobyte.

use std::sync::Arc;

use bevy::{
    asset::{RenderAssetUsages, uuid_handle},
    mesh::MeshTag,
    prelude::*,
    render::storage::ShaderBuffer,
};

use super::{AIR, Palette, VoxelGrid, VoxelId, mesher::Fresh, noise::hash01};

/// As many reveals as can run at once; one more waits for the next change.
pub const SLOTS: usize = 63;

/// Set in the tag of a ghost: its voxels leave where the others arrive.
const GOING: u32 = 1 << 31;

/// A start time no clock reaches: the slot is taken and waits for its meshes.
const NOT_YET: f32 = 1e30;

/// The reveals' clock and slots, as `voxel_vertex.wesl` reads them.
pub const REVEAL_BUFFER: Handle<ShaderBuffer> =
    uuid_handle!("b1d0a6c3-93f4-4f2a-8e57-0c6d2a9e4b77");

#[derive(Clone, Copy)]
struct Slot {
    start: f32,
    seconds: f32,
    /// The model it reveals; the slot is freed when the model stops ([`Revealing`]).
    owner: Option<Entity>,
}

/// The reveals running now.
#[derive(Resource)]
pub struct Reveals {
    /// The player's choice (`graphics.build_animation`): off, changes just appear.
    pub enabled: bool,
    /// Seconds since the first of the running reveals began; 0 while none runs.
    now: f32,
    previous: f32,
    slots: [Option<Slot>; SLOTS],
    /// The buffer is behind the slots.
    stale: bool,
}

impl Default for Reveals {
    fn default() -> Self {
        Self {
            enabled: true,
            now: 0.0,
            previous: 0.0,
            slots: [None; SLOTS],
            stale: true,
        }
    }
}

impl Reveals {
    /// Takes a slot for a reveal of `seconds`, or `None` when they're all taken (the
    /// change then just appears). It starts now, or waits for [`start`](Self::start).
    pub fn open(&mut self, seconds: f32, started: bool, owner: Option<Entity>) -> Option<usize> {
        let slot = self.slots.iter().position(Option::is_none)?;
        self.slots[slot] = Some(Slot {
            start: if started { self.now } else { NOT_YET },
            seconds: seconds.max(0.05),
            owner,
        });
        self.stale = true;
        Some(slot)
    }

    pub fn start(&mut self, slot: usize) {
        if let Some(s) = &mut self.slots[slot] {
            s.start = self.now;
            self.stale = true;
        }
    }

    /// Whether every voxel of the reveal has arrived.
    pub fn done(&self, slot: usize) -> bool {
        self.slots[slot].is_none_or(|s| self.now >= s.start + s.seconds)
    }

    pub fn close(&mut self, slot: usize) {
        self.slots[slot] = None;
        self.stale = true;
    }

    /// What a mesh taking part in `slot` is tagged with; `going` for the ghost.
    pub fn tag(slot: usize, going: bool) -> MeshTag {
        MeshTag::new((slot as u32 + 1) | if going { GOING } else { 0 })
    }

    /// Runs the clock on by `seconds`.
    #[cfg(test)]
    pub fn skip(&mut self, seconds: f32) {
        self.now += seconds;
    }

    fn buffer(&self) -> ShaderBuffer {
        let mut data = vec![self.now, self.previous, 0.0, 0.0];
        for slot in &self.slots {
            // A free slot is one that's over, for a mesh still tagged with it.
            let (start, seconds) = slot.map_or((0.0, 0.0), |s| (s.start, s.seconds));
            data.extend([start, seconds, 0.0, 0.0]);
        }
        ShaderBuffer::new(data, RenderAssetUsages::RENDER_WORLD)
    }
}

pub fn setup(app: &mut App) {
    app.init_resource::<Reveals>();
    app.world_mut()
        .resource_mut::<Assets<ShaderBuffer>>()
        .insert(&REVEAL_BUFFER, Reveals::default().buffer())
        .ok();
}

/// Runs the clock and uploads it. Slots whose model is gone, or no longer being
/// revealed, are freed.
pub fn tick(
    time: Res<Time>,
    mut reveals: ResMut<Reveals>,
    mut buffers: ResMut<Assets<ShaderBuffer>>,
    revealing: Query<&Revealing>,
) {
    let reveals = &mut *reveals;
    for i in 0..SLOTS {
        if let Some(owner) = reveals.slots[i].and_then(|s| s.owner)
            && !revealing.get(owner).is_ok_and(|r| r.slot == i)
        {
            reveals.close(i);
        }
    }
    let running = reveals.slots.iter().any(Option::is_some);
    if !running && reveals.now == 0.0 && !reveals.stale {
        return;
    }
    reveals.previous = reveals.now;
    reveals.now = if running {
        reveals.now + time.delta_secs()
    } else {
        0.0
    };
    if !running {
        reveals.previous = 0.0;
    }
    reveals.stale = false;
    buffers.insert(&REVEAL_BUFFER, reveals.buffer()).ok();
}

/// Reveals the next meshes of the [`VoxelModel`](super::VoxelModel) it's added with,
/// instead of showing them at once.
#[derive(Component, Clone)]
pub struct Reveal {
    /// Each cell's order, in the grid's layout; 0 for the cells that don't change.
    pub arrival: Arc<Vec<u8>>,
    /// What the model had in the cells that change, in its palette then.
    pub ghost: Option<(Arc<VoxelGrid>, Arc<Palette>)>,
    pub seconds: f32,
}

/// A model whose meshes are being revealed in `slot`.
#[derive(Component)]
pub struct Revealing {
    pub slot: usize,
    /// The meshes of what's going, among the model's parts.
    pub ghosts: Vec<Entity>,
}

/// How long `voxels` take to arrive: longer for more, never long.
pub fn seconds_for(voxels: usize) -> f32 {
    (0.6 + (voxels as f32).sqrt() / 150.0).min(2.5)
}

/// A place in the order for `at` (0..1 through the reveal): 1 to 255.
pub fn order(at: f32) -> u8 {
    1 + (at.clamp(0.0, 1.0) * 254.0) as u8
}

fn cell(grid: &VoxelGrid, i: usize) -> IVec3 {
    let size = grid.size();
    let i = i as u32;
    IVec3::new(
        (i % size.x) as i32,
        (i / size.x % size.y) as i32,
        (i / (size.x * size.y)) as i32,
    )
}

/// What a voxel looks like, to tell whether two palettes' voxels are the same.
fn look(palette: &Palette, id: VoxelId) -> [u32; 12] {
    if id == AIR {
        return [0; 12];
    }
    let entry = palette.entry(id);
    let s = &palette.surfaces[entry.surface];
    let (c, e) = (entry.color, s.emissive);
    [
        c.red,
        c.green,
        c.blue,
        s.roughness,
        s.metallic,
        s.reflectance,
        e.red,
        e.green,
        e.blue,
        e.alpha,
        s.alpha,
        1.0,
    ]
    .map(f32::to_bits)
}

impl Reveal {
    /// Every voxel of `grid` arrives. With `shapes` (which shape of its spec made each
    /// cell, [`genverse_rules::build::shape_order`]) they come shape by shape, in the
    /// order the spec lists them, each from the bottom up; without, the whole model
    /// from the bottom up.
    pub fn whole(grid: &VoxelGrid, shapes: Option<&[u16]>) -> Self {
        let arrival = arrivals(grid, shapes, |_| true);
        let voxels = arrival.iter().filter(|a| **a > 0).count();
        Self {
            arrival: Arc::new(arrival),
            ghost: None,
            seconds: seconds_for(voxels),
        }
    }

    /// The cells that differ between `old` and `new` change, and nothing else moves.
    /// `None` when nothing differs. Models of different sizes have no cells in common,
    /// so the new one arrives whole.
    pub fn between(
        old: (&VoxelGrid, &Arc<Palette>),
        new: (&VoxelGrid, &Palette),
        shapes: Option<&[u16]>,
    ) -> Option<Self> {
        if old.0.size() != new.0.size() {
            return Some(Self::whole(new.0, shapes));
        }
        let looks = |palette: &Palette| -> Vec<[u32; 12]> {
            (0..palette.entries.len())
                .map(|id| look(palette, id as VoxelId))
                .collect()
        };
        let (was, is) = (looks(old.1), looks(new.1));
        let changed: Vec<bool> = old
            .0
            .cells()
            .iter()
            .zip(new.0.cells())
            .map(|(a, b)| was[*a as usize] != is[*b as usize])
            .collect();
        let count = changed.iter().filter(|c| **c).count();
        if count == 0 {
            return None;
        }
        let mut arrival = arrivals(new.0, shapes, |i| changed[i]);
        let mut ghost = VoxelGrid::new(old.0.size());
        let height = old.0.size().y.max(1) as f32;
        for (i, id) in old
            .0
            .cells()
            .iter()
            .enumerate()
            .filter(|(i, id)| changed[*i] && **id != AIR)
        {
            let p = cell(old.0, i);
            ghost.set(p, *id);
            // What's only taken away has no place among the shapes: bottom up.
            if arrival[i] == 0 {
                arrival[i] = order(p.y as f32 / height * 0.85 + hash01(p, 17) * 0.15);
            }
        }
        Some(Self {
            arrival: Arc::new(arrival),
            ghost: Some((Arc::new(ghost), old.1.clone())),
            seconds: seconds_for(count),
        })
    }

    /// A cell's part in the reveal, for the mesh of the model as it is now.
    pub fn fresh(&self, grid: &VoxelGrid, p: IVec3) -> Fresh {
        if !grid.contains(p) {
            return Fresh::default();
        }
        let size = grid.size();
        let arrival =
            self.arrival[(p.x as u32 + size.x * (p.y as u32 + size.y * p.z as u32)) as usize];
        let replaces = self
            .ghost
            .as_ref()
            .is_some_and(|(ghost, _)| ghost.get(p) != AIR);
        Fresh {
            arrival,
            swap: arrival > 0 && replaces && grid.get(p) != AIR,
        }
    }
}

/// The order of the filled cells of `grid` that `include` picks.
fn arrivals(grid: &VoxelGrid, shapes: Option<&[u16]>, include: impl Fn(usize) -> bool) -> Vec<u8> {
    let cells = grid.cells();
    let mut arrival = vec![0; cells.len()];
    let taking_part = |i: usize| cells[i] != AIR && include(i);
    let height = grid.size().y.max(1) as f32;
    let shapes = shapes.filter(|s| s.len() == cells.len());
    // Each shape's rank among those with cells here, and how high its cells reach.
    let mut spans: Vec<Option<(i32, i32)>> = Vec::new();
    if let Some(shapes) = shapes {
        for i in (0..cells.len()).filter(|i| taking_part(*i)) {
            let (shape, y) = (shapes[i] as usize, cell(grid, i).y);
            if spans.len() <= shape {
                spans.resize(shape + 1, None);
            }
            spans[shape] = Some(spans[shape].map_or((y, y), |(lo, hi)| (lo.min(y), hi.max(y))));
        }
    }
    let mut rank = 0;
    let mut ranks = Vec::with_capacity(spans.len());
    for span in &spans {
        ranks.push(rank);
        rank += span.is_some() as usize;
    }
    for i in (0..cells.len()).filter(|i| taking_part(*i)) {
        let p = cell(grid, i);
        let jitter = hash01(p, 17);
        let at = match shapes.filter(|_| rank > 1) {
            Some(shapes) => {
                let shape = shapes[i] as usize;
                let (lo, hi) = spans[shape].unwrap_or((0, 0));
                let up = (p.y - lo) as f32 / (hi - lo + 1) as f32;
                (ranks[shape] as f32 + up * 0.8 + jitter * 0.2) / rank as f32
            }
            None => p.y as f32 / height * 0.85 + jitter * 0.15,
        };
        arrival[i] = order(at);
    }
    arrival
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Arc<Palette> {
        Arc::new(crate::world::terrain::palette())
    }

    #[test]
    fn a_whole_model_arrives_from_the_bottom_up() {
        let mut grid = VoxelGrid::new(UVec3::new(2, 16, 2));
        for y in 0..16 {
            grid.set(IVec3::new(0, y, 0), 1);
        }
        let reveal = Reveal::whole(&grid, None);
        let at = |y: i32| reveal.fresh(&grid, IVec3::new(0, y, 0)).arrival;
        assert!(at(0) >= 1 && at(0) < at(8) && at(8) < at(15));
        // Empty cells take no part.
        assert_eq!(reveal.fresh(&grid, IVec3::new(1, 0, 1)), Fresh::default());
    }

    #[test]
    fn shapes_arrive_in_the_order_they_were_written() {
        let mut grid = VoxelGrid::new(UVec3::new(2, 4, 1));
        let mut shapes = vec![0u16; 8];
        // The second shape is the lower one: it still comes second.
        for (p, shape) in [
            (IVec3::new(0, 3, 0), 0),
            (IVec3::new(1, 3, 0), 0),
            (IVec3::new(0, 0, 0), 5),
            (IVec3::new(1, 0, 0), 5),
        ] {
            grid.set(p, 1);
            shapes[(p.x + 2 * p.y) as usize] = shape;
        }
        let reveal = Reveal::whole(&grid, Some(&shapes));
        let at = |p: IVec3| reveal.fresh(&grid, p).arrival;
        assert!(
            at(IVec3::new(0, 3, 0)).max(at(IVec3::new(1, 3, 0)))
                < at(IVec3::new(0, 0, 0)).min(at(IVec3::new(1, 0, 0)))
        );
    }

    #[test]
    fn a_revision_moves_only_what_differs() {
        let palette = palette();
        let mut old = VoxelGrid::new(UVec3::splat(4));
        for p in [
            IVec3::new(0, 0, 0),
            IVec3::new(1, 0, 0),
            IVec3::new(2, 0, 0),
        ] {
            old.set(p, 1);
        }
        let mut new = old.clone();
        new.set(IVec3::new(1, 0, 0), 2); // repainted
        new.set(IVec3::new(2, 0, 0), AIR); // taken away
        new.set(IVec3::new(3, 0, 0), 1); // added
        assert!(Reveal::between((&old, &palette), (&old, &palette), None).is_none());
        let reveal = Reveal::between((&old, &palette), (&new, &palette), None).unwrap();
        let fresh = |x: i32| reveal.fresh(&new, IVec3::new(x, 0, 0));
        assert_eq!(fresh(0), Fresh::default());
        assert!(fresh(1).swap && fresh(1).arrival > 0);
        assert!(!fresh(3).swap && fresh(3).arrival > 0);
        let (ghost, _) = reveal.ghost.as_ref().unwrap();
        let cells = |x: i32| ghost.get(IVec3::new(x, 0, 0));
        assert_eq!([cells(0), cells(1), cells(2), cells(3)], [AIR, 1, 1, AIR]);
        // The one taken away still has its place in the order, for the ghost.
        assert!(reveal.arrival[2] > 0);
    }

    #[test]
    fn slots_start_wait_and_end() {
        let mut reveals = Reveals::default();
        let waiting = reveals.open(1.0, false, None).unwrap();
        let running = reveals.open(1.0, true, None).unwrap();
        assert_ne!(waiting, running);
        reveals.now = 2.0;
        assert!(reveals.done(running) && !reveals.done(waiting));
        reveals.start(waiting);
        reveals.now = 3.5;
        assert!(reveals.done(waiting));
        reveals.close(waiting);
        assert_eq!(reveals.open(1.0, true, None), Some(waiting));
        for _ in 0..SLOTS {
            reveals.open(1.0, true, None);
        }
        assert_eq!(reveals.open(1.0, true, None), None);
    }
}
