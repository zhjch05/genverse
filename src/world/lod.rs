//! Level of detail: the ground is drawn as a quadtree of nodes, each a column of chunks
//! at one level. Level 0 is full detail; a level-`k` voxel stands for `2^k` world
//! voxels on a side, and a level-`k` node covers `2^k` chunk columns on a side, so a
//! node of any level has the same number of voxels to build and draw. Nodes near a
//! loader split into their four children, so detail falls off with distance and each
//! level adds a ring twice as far: the view reaches far for about the same cost per
//! level.
//!
//! Each place in view is drawn at exactly one level. When a node changes level, the
//! old nodes stay until the new ones covering them are all in (see `stream_chunks`).
//!
//! Level 1 is merged from the world voxels it covers; levels past it are sampled from
//! the seed directly ([`Terrain::generate_coarse`]), with the columns that have edits
//! merged from world voxels too, so edits show at every level.

use bevy::{platform::collections::HashSet, prelude::*};

use super::{CHUNK, terrain};
use crate::voxel::{AIR, Palette, VoxelGrid, VoxelId, mesher::mesh_region};
use terrain::{Terrain, VOXEL_SIZE, WORLD_HEIGHT, block, is_solid};

/// How many levels there are: full detail and three coarser ones, at 50 cm, 1 m and
/// 2 m voxels.
pub const LEVELS: u32 = 4;
pub const TOP: u32 = LEVELS - 1;

/// A node splits into its children while a loader is closer to it than this many of
/// its widths: the share of the screen one of its voxels takes stays about the same
/// at every level.
const SPLIT: f32 = 3.0;
/// A split node joins again only past this much farther, so walking along the edge
/// doesn't rebuild nodes back and forth.
const KEEP: f32 = 1.25;

/// A node's width, in meters.
pub fn node_meters(level: u32) -> f32 {
    (CHUNK << level) as f32 * VOXEL_SIZE
}

/// How far the terrain reaches, in meters: top-level nodes this close are drawn.
pub const VIEW_DISTANCE: f32 = 2.0 * SPLIT * (CHUNK << TOP) as f32 * VOXEL_SIZE;

/// World voxels per voxel at `level`, along each axis.
pub fn step(level: u32) -> i32 {
    1 << level
}

/// World voxels a chunk at `level` spans, along each axis.
pub fn span(level: u32) -> i32 {
    CHUNK << level
}

/// Chunks in a column at `level`.
pub fn vertical(level: u32) -> i32 {
    ((WORLD_HEIGHT + span(level) - 1) / span(level)).max(1)
}

/// A column of chunks at one level, in units of its own width.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Node {
    pub level: u32,
    pub xz: IVec2,
}

impl Node {
    pub fn new(level: u32, xz: IVec2) -> Self {
        Self { level, xz }
    }

    /// The level-0 node (one chunk column) holding chunk column `column`.
    pub fn of_column(column: IVec2) -> Self {
        Self::new(0, column)
    }

    pub fn parent(self) -> Self {
        Self::new(self.level + 1, self.xz.div_euclid(IVec2::splat(2)))
    }

    pub fn children(self) -> [Self; 4] {
        let c = self.xz * 2;
        [IVec2::ZERO, IVec2::X, IVec2::Y, IVec2::ONE].map(|d| Self::new(self.level - 1, c + d))
    }

    /// This node, or the ancestor of it at `level`.
    pub fn at(self, level: u32) -> Self {
        Self::new(level, self.xz.div_euclid(IVec2::splat(1 << (level - self.level))))
    }

    /// Whether the two cover any ground in common: one holds the other.
    pub fn overlaps(self, other: Self) -> bool {
        if self.level >= other.level { other.at(self.level) == self } else { self.at(other.level) == other }
    }

    /// Its chunks, keyed by chunk coordinates at its level, bottom first.
    pub fn chunks(self) -> impl Iterator<Item = IVec3> {
        (0..vertical(self.level)).map(move |y| IVec3::new(self.xz.x, y, self.xz.y))
    }

    /// Distance in meters from `p` (meters, on the ground plane) to the nearest point
    /// of the node.
    pub fn distance(self, p: Vec2) -> f32 {
        let size = node_meters(self.level);
        let min = self.xz.as_vec2() * size;
        (p - p.clamp(min, min + size)).length()
    }
}

/// The nodes to draw for loaders at `loaders` (meters, on the ground plane), and the
/// nodes split to get them. `split` is last time's, for the margin a split node keeps;
/// `drawn` is what's drawn now, which keeps a margin at the edge of the view.
pub fn select(loaders: &[Vec2], split: &HashSet<Node>, drawn: impl Fn(Node) -> bool) -> (HashSet<Node>, HashSet<Node>) {
    let (mut leaves, mut splits) = (HashSet::default(), HashSet::default());
    let top = node_meters(TOP);
    let reach = (VIEW_DISTANCE * KEEP / top).ceil() as i32 + 1;
    let mut roots = HashSet::<Node>::default();
    for p in loaders {
        let center = (*p / top).floor().as_ivec2();
        for dz in -reach..=reach {
            for dx in -reach..=reach {
                roots.insert(Node::new(TOP, center + IVec2::new(dx, dz)));
            }
        }
    }
    let distance = |node: Node| loaders.iter().map(|p| node.distance(*p)).fold(f32::INFINITY, f32::min);
    let mut stack: Vec<Node> = roots
        .into_iter()
        .filter(|root| {
            let had = split.contains(root) || drawn(*root);
            distance(*root) < VIEW_DISTANCE * if had { KEEP } else { 1.0 }
        })
        .collect();
    while let Some(node) = stack.pop() {
        let limit = SPLIT * node_meters(node.level) * if split.contains(&node) { KEEP } else { 1.0 };
        if node.level > 0 && distance(node) < limit {
            splits.insert(node);
            stack.extend(node.children());
        } else {
            leaves.insert(node);
        }
    }
    (leaves, splits)
}

const SOLID: u8 = 1;
const BARK: u8 = 2;
const WATER: u8 = 4;

fn kinds() -> [u8; 256] {
    std::array::from_fn(|id| {
        let id = id as VoxelId;
        if is_solid(id) {
            SOLID | if block::BARK.contains(&id) { BARK } else { 0 }
        } else if id == block::WATER {
            WATER
        } else {
            0
        }
    })
}

/// One coarse voxel from the world voxels it covers, top layer first, as kinds from
/// `kinds`: solid when at least half are, taking the topmost solid's material so grass
/// stays on top; trunks stay connected from a thread of bark; water when solids and
/// water make a majority.
fn merge(fine: &[VoxelId], step: i32, kinds: &[u8; 256]) -> VoxelId {
    let (mut solid, mut bark, mut water, mut top) = (0, 0, 0, AIR);
    for &id in fine {
        let kind = kinds[id as usize];
        if kind & SOLID != 0 {
            solid += 1;
            if top == AIR {
                top = id;
            }
            if kind & BARK != 0 {
                bark += 1;
            }
        } else if kind & WATER != 0 {
            water += 1;
        }
    }
    let half = fine.len() as i32 / 2;
    if solid >= half {
        top
    } else if bark >= step {
        block::BARK[0]
    } else if water > 0 && solid + water >= half {
        block::WATER
    } else {
        AIR
    }
}

/// Merges a grid of world voxels into one of coarse voxels, `step` times smaller;
/// only the columns `columns` passes (coarse x and z), when given.
fn downsample(fine: &VoxelGrid, step: i32, columns: Option<&dyn Fn(IVec2) -> bool>) -> VoxelGrid {
    let kinds = kinds();
    let size = fine.size() / step as u32;
    let mut coarse = VoxelGrid::new(size);
    let mut cell = Vec::with_capacity((step * step * step) as usize);
    for z in 0..size.z as i32 {
        for x in 0..size.x as i32 {
            if columns.is_some_and(|f| !f(IVec2::new(x, z))) {
                continue;
            }
            for y in 0..size.y as i32 {
                cell.clear();
                // Top layer first, so the first solid seen is the topmost.
                for dy in (0..step).rev() {
                    for dz in 0..step {
                        for dx in 0..step {
                            cell.push(fine.get(IVec3::new(x, y, z) * step + IVec3::new(dx, dy, dz)));
                        }
                    }
                }
                coarse.set(IVec3::new(x, y, z), merge(&cell, step, &kinds));
            }
        }
    }
    coarse
}

/// The world voxels a chunk at `level` is made from: its span plus one of its voxels
/// of border, so it meshes without seams against its neighbors at that level.
pub fn coarse_region(level: u32, key: IVec3) -> (IVec3, IVec3) {
    let min = key * span(level) - IVec3::splat(step(level));
    (min, min + IVec3::splat(span(level) + 2 * step(level)))
}

/// The chunks at `level` (1 and up) whose region holds any world voxel in `lo..=hi`.
pub fn coarse_keys(level: u32, lo: IVec3, hi: IVec3) -> impl Iterator<Item = IVec3> {
    let (span, step) = (IVec3::splat(span(level)), IVec3::splat(step(level)));
    let (lo, hi) = ((lo - step).div_euclid(span), (hi + step).div_euclid(span));
    (lo.z..=hi.z).flat_map(move |z| (lo.y..=hi.y).flat_map(move |y| (lo.x..=hi.x).map(move |x| IVec3::new(x, y, z))))
}

/// The voxels of chunk `key` at `level` (1 and up) with its border: generated from
/// the seed, with `edits` (world voxels in its region) merged in.
fn coarse_voxels(terrain: Terrain, level: u32, key: IVec3, edits: &[(IVec3, VoxelId)]) -> VoxelGrid {
    let (min, max) = coarse_region(level, key);
    let step = step(level);
    if level == 1 {
        let mut fine = terrain.generate(min, (max - min).as_uvec3());
        for (p, id) in edits {
            fine.set(*p - min, *id);
        }
        return downsample(&fine, step, None);
    }
    let cells = (max - min) / step;
    let mut coarse = terrain.generate_coarse(min / step, cells.as_uvec3(), step);
    if edits.is_empty() {
        return coarse;
    }
    // The columns with edits, merged from world voxels: just the box around them.
    let columns: HashSet<IVec2> = edits.iter().map(|(p, _)| (p.xz() - min.xz()).div_euclid(IVec2::splat(step))).collect();
    let (lo, hi) = columns.iter().fold((IVec2::MAX, IVec2::MIN), |(lo, hi), c| (lo.min(*c), hi.max(*c)));
    let fine_min = min + IVec3::new(lo.x, 0, lo.y) * step;
    let fine_size = IVec3::new(hi.x - lo.x + 1, cells.y, hi.y - lo.y + 1) * step;
    let mut fine = terrain.generate(fine_min, fine_size.as_uvec3());
    for (p, id) in edits {
        fine.set(*p - fine_min, *id);
    }
    let edited = |c: IVec2| columns.contains(&(c + lo));
    let merged = downsample(&fine, step, Some(&edited));
    for c in &columns {
        for y in 0..cells.y {
            let at = IVec3::new(c.x, y, c.y);
            coarse.set(at, merged.get(at - IVec3::new(lo.x, 0, lo.y)));
        }
    }
    coarse
}

/// Generates chunk `key` at `level` (1 and up) from the seed and the `edits` in its
/// region, and meshes it. Meshes are in that level's voxels, relative to the chunk's
/// corner.
pub fn build_coarse(terrain: Terrain, palette: &Palette, level: u32, key: IVec3, edits: &[(IVec3, VoxelId)]) -> Vec<(usize, Mesh)> {
    let coarse = coarse_voxels(terrain, level, key, edits);
    let origin = key * CHUNK;
    mesh_region(|p| coarse.get(p - origin + IVec3::ONE), palette, origin, origin + IVec3::splat(CHUNK))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nodes_nest() {
        let n = Node::new(2, IVec2::new(-1, 3));
        assert!(n.children().iter().all(|c| c.parent() == n && c.overlaps(n) && n.overlaps(*c)));
        assert_eq!(Node::new(0, IVec2::new(-3, 13)).at(2), n);
        assert!(!Node::new(2, IVec2::new(0, 3)).overlaps(Node::new(0, IVec2::new(-1, 12))));
        assert_eq!(n.distance(Vec2::new(-10.0, 100.0)), 0.0);
    }

    /// The nodes drawn cover the view once each, finer toward the loader.
    #[test]
    fn selection_tiles_the_view() {
        let (leaves, splits) = select(&[Vec2::new(3.0, -7.0)], &HashSet::default(), |_| false);
        for a in &leaves {
            for b in &leaves {
                assert!(a == b || !a.overlaps(*b), "{a:?} and {b:?} overlap");
            }
            assert!(!splits.contains(a));
        }
        let at = |p: Vec2| leaves.iter().find(|n| n.distance(p) == 0.0 && n.distance(p + 0.01) == 0.0).map(|n| n.level);
        assert_eq!(at(Vec2::new(3.0, -7.0)), Some(0));
        assert_eq!(at(Vec2::new(80.0, -7.0)), Some(1));
        assert_eq!(at(Vec2::new(-150.0, -7.0)), Some(2));
        assert_eq!(at(Vec2::new(3.0, 250.0)), Some(3));
        assert_eq!(at(Vec2::new(3.0, VIEW_DISTANCE + 100.0)), None);
    }

    #[test]
    fn split_nodes_keep_a_margin() {
        let node = Node::new(1, IVec2::new(5, 0));
        let edge = node.distance(Vec2::ZERO);
        // Just past where it would split: whole when it was, split when it already was.
        let p = Vec2::new(-(SPLIT * node_meters(1) - edge) - 1.0, 0.0);
        let (leaves, _) = select(&[p], &HashSet::default(), |_| false);
        assert!(leaves.contains(&node));
        let (_, splits) = select(&[p], &[node].into_iter().collect(), |_| false);
        assert!(splits.contains(&node));
    }

    #[test]
    fn merging_keeps_ground_tops_trunks_and_water() {
        let kinds = kinds();
        // Ground in the bottom layer, grass on half of the top: solid, grass on top.
        let mut cell = [AIR; 8];
        cell[4..].fill(block::DIRT[0]);
        cell[0] = block::GRASS[0];
        assert_eq!(merge(&cell, 2, &kinds), block::GRASS[0]);
        // A trunk clipping one corner column still shows.
        let mut trunk = [AIR; 8];
        trunk[3] = block::BARK[1];
        trunk[7] = block::BARK[1];
        assert_eq!(merge(&trunk, 2, &kinds), block::BARK[0]);
        // A seabed voxel under water is water; a lone flower is air.
        let mut sea = [block::WATER; 8];
        sea[4] = block::SAND[0];
        assert_eq!(merge(&sea, 2, &kinds), block::WATER);
        let mut flower = [AIR; 8];
        flower[4] = block::FLOWERS[0];
        assert_eq!(merge(&flower, 2, &kinds), AIR);
    }

    #[test]
    fn coarse_chunks_mesh_the_seed_and_edits() {
        let terrain = Terrain { seed: super::super::SEED, flat: false };
        let palette = terrain::palette();
        for level in 1..LEVELS {
            assert!(!build_coarse(terrain, &palette, level, IVec3::new(3, 0, -2), &[]).is_empty());
        }
        // A pillar of stone up to the sky shows at level 2.
        let key = IVec3::ZERO;
        let (min, _) = coarse_region(2, key);
        let column = IVec2::new(40, 40);
        let edits: Vec<_> = (0..WORLD_HEIGHT)
            .flat_map(|y| (0..4).flat_map(move |dz| (0..4).map(move |dx| (IVec3::new(column.x + dx, y, column.y + dz), block::STONE[0]))))
            .collect();
        let voxels = coarse_voxels(terrain, 2, key, &edits);
        let top = (WORLD_HEIGHT - 1) / 4 - min.y / 4;
        assert_eq!(voxels.get(IVec3::new(column.x / 4 - min.x / 4, top, column.y / 4 - min.z / 4)), block::STONE[0]);
    }
}
