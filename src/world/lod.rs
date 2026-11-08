//! Level of detail: terrain past the full-detail area comes from coarse chunks, whose
//! voxels are 2x2x2 world voxels merged into one, so the view reaches twice as far for
//! a fraction of the cost.
//!
//! The ground is split into cells of 2x2 chunk columns, the footprint of one coarse
//! chunk. A cell is drawn at one level at a time, so the levels never overlap; when
//! a cell changes level, its old chunks stay until the new ones are all in.

use bevy::prelude::*;

use super::{CHUNK, terrain};
use crate::voxel::{AIR, Palette, VoxelGrid, VoxelId, mesher::mesh_region};
use terrain::{Terrain, VOXEL_SIZE, WORLD_HEIGHT, block, is_solid};

/// World voxels per coarse voxel, along each axis.
pub const COARSE: i32 = 2;
/// Chunk columns per cell, along each axis: a coarse chunk's footprint.
pub const CELL: i32 = COARSE;
/// World voxels a coarse chunk spans, along each axis.
pub const COARSE_SPAN: i32 = CHUNK * COARSE;
pub const COARSE_VERTICAL: i32 = WORLD_HEIGHT / COARSE_SPAN;
/// Cells this close (squared distance, in cells) to a loader are in full detail:
/// a radius of 3.5 cells, the 7 chunks the world had before it had levels.
const NEAR: i32 = 12;
/// A full-detail cell turns coarse only past this, so walking along the edge doesn't
/// rebuild cells back and forth.
const NEAR_KEEP: i32 = 20;
/// Cells this close are drawn at all: 8 cells, 128 m.
const FAR: i32 = 64;
const FAR_KEEP: i32 = 81;

/// How far the terrain reaches, in meters.
pub const VIEW_DISTANCE: f32 = 8.0 * (CELL * CHUNK) as f32 * VOXEL_SIZE;
/// Cells from a loader to check, along each axis.
pub const SCAN: i32 = 9;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lod {
    Near,
    Far,
}

pub fn cell_of_column(column: IVec2) -> IVec2 {
    column.div_euclid(IVec2::splat(CELL))
}

/// The level a cell at squared distance `d2` (in cells) from the nearest loader should
/// be drawn at, given the one it has now; `None` past the view.
pub fn wanted(d2: i32, now: Option<Lod>) -> Option<Lod> {
    let near = if now == Some(Lod::Near) { NEAR_KEEP } else { NEAR };
    let far = if now.is_some() { FAR_KEEP } else { FAR };
    if d2 <= near {
        Some(Lod::Near)
    } else if d2 <= far {
        Some(Lod::Far)
    } else {
        None
    }
}

/// One coarse voxel from the 2x2x2 world voxels it covers: solid when most of them
/// are, taking the topmost solid's material so grass stays on top; trunks stay
/// connected from two bark voxels; water when solids and water make a majority.
fn merge(fine: &VoxelGrid, at: IVec3) -> VoxelId {
    let (mut solid, mut bark, mut water, mut top) = (0, 0, 0, AIR);
    // Top layer first, so the first solid seen is the topmost.
    for y in (0..COARSE).rev() {
        for z in 0..COARSE {
            for x in 0..COARSE {
                let id = fine.get(at + IVec3::new(x, y, z));
                if is_solid(id) {
                    solid += 1;
                    if top == AIR {
                        top = id;
                    }
                    if block::BARK.contains(&id) {
                        bark += 1;
                    }
                } else if id == block::WATER {
                    water += 1;
                }
            }
        }
    }
    let half = COARSE * COARSE * COARSE / 2;
    if solid >= half {
        top
    } else if bark >= 2 {
        block::BARK[0]
    } else if water > 0 && solid + water >= half {
        block::WATER
    } else {
        AIR
    }
}

/// Merges a grid of world voxels into one of coarse voxels, `COARSE` times smaller.
fn downsample(fine: &VoxelGrid) -> VoxelGrid {
    let size = fine.size() / COARSE as u32;
    let mut coarse = VoxelGrid::new(size);
    for z in 0..size.z as i32 {
        for y in 0..size.y as i32 {
            for x in 0..size.x as i32 {
                let p = IVec3::new(x, y, z);
                coarse.set(p, merge(fine, p * COARSE));
            }
        }
    }
    coarse
}

/// The world voxels a coarse chunk is made from: its span plus one coarse voxel of
/// border, so it meshes without seams against its coarse neighbors.
pub fn coarse_region(key: IVec3) -> (IVec3, IVec3) {
    let min = key * COARSE_SPAN - IVec3::splat(COARSE);
    (min, min + IVec3::splat(COARSE_SPAN + 2 * COARSE))
}

/// Generates coarse chunk `key` from the seed and the `edits` in its region, and
/// meshes it. Meshes are in coarse voxels, relative to the chunk's corner.
pub fn build_coarse(terrain: Terrain, palette: &Palette, key: IVec3, edits: &[(IVec3, VoxelId)]) -> Vec<(usize, Mesh)> {
    let (min, max) = coarse_region(key);
    let mut fine = terrain.generate(min, (max - min).as_uvec3());
    for (p, id) in edits {
        fine.set(*p - min, *id);
    }
    let coarse = downsample(&fine);
    let origin = key * CHUNK;
    mesh_region(|p| coarse.get(p - origin + IVec3::ONE), palette, origin, origin + IVec3::splat(CHUNK))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_switch_with_a_margin() {
        assert_eq!(wanted(0, None), Some(Lod::Near));
        assert_eq!(wanted(NEAR + 1, None), Some(Lod::Far));
        // A full-detail cell stays so a little past the edge.
        assert_eq!(wanted(NEAR + 1, Some(Lod::Near)), Some(Lod::Near));
        assert_eq!(wanted(NEAR_KEEP + 1, Some(Lod::Near)), Some(Lod::Far));
        assert_eq!(wanted(FAR + 1, None), None);
        assert_eq!(wanted(FAR + 1, Some(Lod::Far)), Some(Lod::Far));
        assert_eq!(wanted(FAR_KEEP + 1, Some(Lod::Far)), None);
    }

    #[test]
    fn merging_keeps_ground_tops_trunks_and_water() {
        let mut fine = VoxelGrid::new(UVec3::splat(2));
        // Ground in the bottom layer, grass on half of the top: solid, grass on top.
        for x in 0..2 {
            for z in 0..2 {
                fine.set(IVec3::new(x, 0, z), block::DIRT[0]);
            }
        }
        fine.set(IVec3::new(0, 1, 0), block::GRASS[0]);
        assert_eq!(merge(&fine, IVec3::ZERO), block::GRASS[0]);

        // A trunk clipping one corner column still shows.
        let mut trunk = VoxelGrid::new(UVec3::splat(2));
        trunk.set(IVec3::new(1, 0, 1), block::BARK[1]);
        trunk.set(IVec3::new(1, 1, 1), block::BARK[1]);
        assert_eq!(merge(&trunk, IVec3::ZERO), block::BARK[0]);

        // A seabed voxel under water is water; a lone flower is air.
        let mut sea = VoxelGrid::new(UVec3::splat(2));
        for x in 0..2 {
            for z in 0..2 {
                sea.set(IVec3::new(x, 1, z), block::WATER);
            }
        }
        sea.set(IVec3::ZERO, block::SAND[0]);
        assert_eq!(merge(&sea, IVec3::ZERO), block::WATER);
        let mut flower = VoxelGrid::new(UVec3::splat(2));
        flower.set(IVec3::ZERO, block::FLOWERS[0]);
        assert_eq!(merge(&flower, IVec3::ZERO), AIR);
    }

    #[test]
    fn coarse_chunks_mesh_the_seed() {
        let terrain = Terrain { seed: super::super::SEED };
        let meshes = build_coarse(terrain, &terrain::palette(), IVec3::new(3, 0, -2), &[]);
        assert!(!meshes.is_empty());
    }
}
