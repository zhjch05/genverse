//! Deterministic terrain: every voxel is a pure function of its position and the seed,
//! so chunks generate independently (and in parallel) without seams.

use bevy_color::prelude::*;
use bevy_math::prelude::*;

use crate::voxel::{
    AIR, Palette, PaletteEntry, Surface, VoxelGrid, VoxelId,
    noise::{ValueGrid, fbm, fbm_xz, hash01, value, value_xz},
};

/// World voxels are 25 cm: fine enough to read as "HD", coarse enough to build with.
pub const VOXEL_SIZE: f32 = 0.25;
pub const SEA_LEVEL: i32 = 30;
/// The world is this many voxels tall; below is bedrock, above is sky.
pub const WORLD_HEIGHT: i32 = 128;

/// Block ids, in palette order.
pub mod block {
    use crate::voxel::VoxelId;

    pub const GRASS: [VoxelId; 3] = [1, 2, 3];
    pub const DIRT: [VoxelId; 2] = [4, 5];
    pub const STONE: [VoxelId; 3] = [6, 7, 8];
    pub const SAND: [VoxelId; 2] = [9, 10];
    pub const WATER: VoxelId = 11;
    pub const BARK: [VoxelId; 2] = [12, 13];
    pub const LEAVES: [VoxelId; 3] = [14, 15, 16];
    pub const TUFT: VoxelId = 17;
    pub const FLOWERS: [VoxelId; 3] = [18, 19, 20];
    pub const SNOW: VoxelId = 21;
}

pub fn palette() -> Palette {
    let surface = |name: &str, roughness: f32, alpha: f32| Surface {
        name: name.into(),
        roughness,
        metallic: 0.0,
        reflectance: if alpha < 1.0 { 0.6 } else { 0.35 },
        emissive: LinearRgba::BLACK,
        alpha,
    };
    let surfaces = vec![
        surface("ground", 0.92, 1.0),
        surface("foliage", 0.75, 1.0),
        surface("water", 0.03, 0.7),
    ];
    let entry = |name: &str, hex: &str, surface: usize, variation: f32| PaletteEntry {
        name: name.into(),
        color: Srgba::hex(hex).unwrap().into(),
        surface,
        variation,
    };
    let entries = vec![
        entry("air", "#000000", 0, 0.0),
        entry("grass_light", "#9cc464", 0, 0.05),
        entry("grass", "#7fb04f", 0, 0.05),
        entry("grass_dark", "#679a42", 0, 0.05),
        entry("dirt", "#8d6545", 0, 0.06),
        entry("dirt_dark", "#71503a", 0, 0.06),
        entry("stone_light", "#a19c94", 0, 0.05),
        entry("stone", "#8a857e", 0, 0.05),
        entry("stone_dark", "#6f6b66", 0, 0.05),
        entry("sand", "#e3d3a0", 0, 0.04),
        entry("sand_dark", "#d2bf8a", 0, 0.04),
        entry("water", "#4aa3c4", 2, 0.02),
        entry("bark", "#6b4a35", 0, 0.08),
        entry("bark_dark", "#533826", 0, 0.08),
        entry("leaves_light", "#7dbb4f", 1, 0.06),
        entry("leaves", "#5f9f3e", 1, 0.06),
        entry("leaves_dark", "#4a8333", 1, 0.06),
        entry("tuft", "#a6cf6b", 1, 0.08),
        entry("flower_red", "#e5534b", 1, 0.03),
        entry("flower_yellow", "#f4cf4b", 1, 0.03),
        entry("flower_white", "#f3efe4", 1, 0.03),
        entry("snow", "#f4f6f8", 0, 0.02),
    ];
    Palette { surfaces, entries }
}

/// Whether this is ground (as opposed to water, plants, trees, or air).
pub fn is_ground(id: VoxelId) -> bool {
    block::GRASS.contains(&id)
        || block::DIRT.contains(&id)
        || block::STONE.contains(&id)
        || block::SAND.contains(&id)
        || id == block::SNOW
}

/// Whether the player collides with this block. Water and small plants are walk-through.
pub fn is_solid(id: VoxelId) -> bool {
    id != AIR && id != block::WATER && id != block::TUFT && !block::FLOWERS.contains(&id)
}

pub fn pick<const N: usize>(ids: [VoxelId; N], t: f32) -> VoxelId {
    ids[((t.clamp(0.0, 0.999)) * N as f32) as usize]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Terrain {
    pub seed: u32,
    /// An empty world: level grass everywhere, no water, trees or plants.
    pub flat: bool,
}

/// The ground's top voxel in a flat world: a little above the sea.
pub const FLAT_GROUND: i32 = SEA_LEVEL + 4;

const TREE_CELL: i32 = 18;
/// Farthest a tree's leaves reach from its trunk, including noise.
const TREE_REACH: i32 = 14;

impl Terrain {
    /// Height of the top solid voxel in column `(x, z)`.
    pub fn height(&self, x: i32, z: i32) -> i32 {
        if self.flat {
            return FLAT_GROUND;
        }
        let p = Vec2::new(x as f32, z as f32);
        let continent = fbm_xz(p / 260.0, self.seed);
        let hills = fbm_xz(p / 70.0, self.seed + 10);
        let ridges = 1.0 - (fbm_xz(p / 110.0, self.seed + 20) * 2.0 - 1.0).abs();
        let mountain = ((continent - 0.45) * 2.5).clamp(0.0, 1.0);
        let h = 22.0
            + continent * 26.0
            + (hills - 0.5) * 22.0
            + ridges.powi(3) * mountain * 48.0
            + (value_xz(p / 9.0, self.seed + 30) - 0.5) * 3.0;
        (h.round() as i32).clamp(2, WORLD_HEIGHT - 24)
    }

    /// Fills a grid covering world voxels `min .. min + size`.
    pub fn generate(&self, min: IVec3, size: UVec3) -> VoxelGrid {
        let mut grid = VoxelGrid::new(size);
        let max = min + size.as_ivec3();
        let local = |p: IVec3| p - min;
        let heights: Vec<i32> = (min.z..max.z)
            .flat_map(|z| (min.x..max.x).map(move |x| (x, z)))
            .map(|(x, z)| self.height(x, z))
            .collect();
        let height = |x: i32, z: i32| heights[((z - min.z) * size.x as i32 + (x - min.x)) as usize];

        // Terrain columns, with the noise that picks their blocks laid out over the part
        // of the grid they reach.
        let top = heights.iter().max().map_or(min.y, |h| (h + 1).clamp(min.y, max.y));
        let reach = IVec3::new(size.x as i32, top - min.y, size.z as i32);
        let patches = ValueGrid::new(min, reach, 5.0, self.seed + 40);
        let stones = ValueGrid::new(min, reach, 4.0, self.seed + 42);
        for z in min.z..max.z {
            for x in min.x..max.x {
                let h = height(x, z);
                let beach = h <= SEA_LEVEL + 1 && !self.flat;
                // Below y = 0 is solid stone, so the world has no visible underside.
                for y in min.y..max.y.min(h + 1).max(min.y) {
                    let p = IVec3::new(x, y, z);
                    let depth = h - y;
                    let id = self.ground_block(p, depth, beach, || patches.get(local(p)), || stones.get(local(p)));
                    grid.set(local(p), id);
                }
                for y in (h + 1).max(min.y)..=SEA_LEVEL.min(max.y - 1) {
                    grid.set(local(IVec3::new(x, y, z)), block::WATER);
                }
            }
        }

        if self.flat {
            return grid;
        }
        // Trees, one candidate per jittered cell.
        for cell in Self::tree_cells(min.xz(), max.xz()) {
            self.place_tree(&mut grid, min, cell);
        }

        // Grass tufts and flowers on exposed grass.
        for z in min.z..max.z {
            for x in min.x..max.x {
                let h = height(x, z);
                let top = IVec3::new(x, h, z);
                let above = top + IVec3::Y;
                if !block::GRASS.contains(&get(&grid, min, top)) || get(&grid, min, above) != AIR {
                    continue;
                }
                let roll = hash01(top, self.seed + 50);
                if roll < 0.006 {
                    let flower = pick(block::FLOWERS, value(top.as_vec3() / 20.0, self.seed + 51));
                    grid.set(local(above), flower);
                } else if roll < 0.03 {
                    grid.set(local(above), block::TUFT);
                }
            }
        }
        grid
    }

    /// The ground block at `p`, `depth` voxels under the top of its column; `beach`
    /// when the column is at the shore. `patch` and `stone` give the noise that picks
    /// among sands and dirts, and among stones, at `p`.
    fn ground_block(&self, p: IVec3, depth: i32, beach: bool, patch: impl Fn() -> f32, stone: impl Fn() -> f32) -> VoxelId {
        let h = p.y + depth;
        if depth == 0 && h > 78 {
            block::SNOW
        } else if beach && depth < 4 {
            pick(block::SAND, patch())
        } else if depth == 0 {
            pick(block::GRASS, fbm(p.as_vec3() / 12.0, self.seed + 41) * 1.4 - 0.2)
        } else if depth < 4 {
            pick(block::DIRT, patch())
        } else {
            pick(block::STONE, stone())
        }
    }

    /// The tree of tree cell `cell`, if it has one: the ground voxel its trunk stands on
    /// and the key that picks its variation.
    fn tree_in(&self, cell: IVec2) -> Option<(IVec3, IVec3)> {
        let key = IVec3::new(cell.x, 0, cell.y);
        // Groves and open meadows rather than uniform forest.
        let grove = fbm_xz(cell.as_vec2() / 6.0, self.seed + 59);
        if hash01(key, self.seed + 60) > (grove - 0.35) * 1.4 {
            return None;
        }
        let jitter = |k: u32| (hash01(key, self.seed + k) * (TREE_CELL - 6) as f32) as i32 + 3;
        let (x, z) = (cell.x * TREE_CELL + jitter(61), cell.y * TREE_CELL + jitter(62));
        let ground = self.height(x, z);
        let steep = (self.height(x + 2, z) - self.height(x - 2, z)).abs()
            + (self.height(x, z + 2) - self.height(x, z - 2)).abs();
        if ground <= SEA_LEVEL + 1 || ground > 70 || steep > 5 {
            return None;
        }
        Some((IVec3::new(x, ground, z), key))
    }

    /// The tree cells whose trees can reach world columns `min..max`.
    fn tree_cells(min: IVec2, max: IVec2) -> impl Iterator<Item = IVec2> {
        let cells = |a: i32, b: i32| (a - TREE_REACH).div_euclid(TREE_CELL)..=(b + TREE_REACH).div_euclid(TREE_CELL);
        cells(min.y, max.y).flat_map(move |cz| cells(min.x, max.x).map(move |cx| IVec2::new(cx, cz)))
    }

    fn place_tree(&self, grid: &mut VoxelGrid, min: IVec3, cell: IVec2) {
        let Some((base, key)) = self.tree_in(cell) else {
            return;
        };
        let grid_max = min + grid.size().as_ivec3();
        if base.y + 1 >= grid_max.y || base.y + 37 < min.y {
            return;
        }
        self.grow_tree(grid, min, base, key);
    }

    /// The shape of the tree whose 2x2 trunk stands on `base`: the top of its trunk,
    /// and its crown's leaf blobs as centers and radii.
    fn tree_shape(&self, base: IVec3, key: IVec3) -> (IVec3, [(Vec3, Vec3); 2]) {
        let trunk_height = 13 + (hash01(key, self.seed + 63) * 8.0) as i32;
        let crown = IVec3::new(base.x, base.y + trunk_height, base.z);
        let radius = 5.5 + hash01(key, self.seed + 65) * 2.5;
        let blobs = [
            (crown.as_vec3() + Vec3::new(1.0, 0.0, 1.0), Vec3::new(radius, radius * 0.75, radius)),
            (crown.as_vec3() + Vec3::new(1.0, radius * 0.6, 1.0), Vec3::splat(radius * 0.65)),
        ];
        (crown, blobs)
    }

    /// Whether the point `q` (in world voxels) is inside a leaf blob, and if so the
    /// leaves there.
    fn leaf_at(&self, q: Vec3, (center, radii): (Vec3, Vec3)) -> Option<VoxelId> {
        // The wobble is within ±0.35, so past that it can't reach.
        let reach = ((q - center) / radii).length();
        if reach > 1.35 {
            return None;
        }
        let wobble = (fbm(q / 3.0, self.seed + 66) - 0.5) * 0.7;
        if reach > 1.0 + wobble {
            return None;
        }
        let shade = fbm(q / 4.0, self.seed + 67) + (q.y - center.y) / radii.y * 0.3;
        Some(pick(block::LEAVES, 1.0 - shade))
    }

    /// Grows a tree whose 2x2 trunk stands on `base` (the ground voxel), into the part
    /// of `grid` (covering world voxels from `min`) it reaches. `key` picks its variation.
    pub fn grow_tree(&self, grid: &mut VoxelGrid, min: IVec3, base: IVec3, key: IVec3) {
        let (x, ground, z) = (base.x, base.y, base.z);
        let (crown, blobs) = self.tree_shape(base, key);

        // 2x2 trunk.
        for y in ground + 1..crown.y {
            for (dx, dz) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let p = IVec3::new(x + dx, y, z + dz);
                set_if_air(grid, min, p, pick(block::BARK, hash01(p, 64)));
            }
        }

        // A couple of noisy leaf blobs around the crown.
        for blob in blobs {
            let (center, radii) = blob;
            // Only visit the part of the blob inside this grid.
            let lo = (center - radii - 2.0).floor().as_ivec3().max(min);
            let hi = (center + radii + 2.0).ceil().as_ivec3().min(min + grid.size().as_ivec3() - 1);
            for pz in lo.z..=hi.z {
                for py in lo.y..=hi.y {
                    for px in lo.x..=hi.x {
                        let p = IVec3::new(px, py, pz);
                        if let Some(leaves) = self.leaf_at(p.as_vec3() + 0.5, blob) {
                            set_if_air(grid, min, p, leaves);
                        }
                    }
                }
            }
        }
    }

    /// Like [`generate`](Self::generate), in coarse voxels of `step` world voxels on a
    /// side: coarse cells `min .. min + size`, each standing for the world voxels
    /// `cell * step ..` of it. Sampled rather than built voxel by voxel, so a coarse
    /// grid costs about the same whatever its step, for terrain far away.
    ///
    /// A cell is ground when most of it is under its columns' mean height, and has the
    /// block of its topmost ground voxel; water when most of it is under the sea; leaves
    /// where its center is in a crown, and bark on a trunk while trunks are still a cell
    /// wide. Tufts and flowers are too small to show.
    pub fn generate_coarse(&self, min: IVec3, size: UVec3, step: i32) -> VoxelGrid {
        let mut grid = VoxelGrid::new(size);
        let max = min + size.as_ivec3();
        let half = step as f32 / 2.0;
        // Sample points across a cell's columns, per axis.
        let samples = step.min(4);
        let offsets: Vec<i32> = (0..samples).map(|i| (2 * i + 1) * step / (2 * samples)).collect();
        for cz in min.z..max.z {
            for cx in min.x..max.x {
                let mut sum = 0;
                for dz in &offsets {
                    for dx in &offsets {
                        sum += self.height(cx * step + dx, cz * step + dz);
                    }
                }
                let mean = sum as f32 / (samples * samples) as f32;
                let h = mean.round() as i32;
                let beach = h <= SEA_LEVEL + 1 && !self.flat;
                let center = IVec2::new(cx * step + step / 2, cz * step + step / 2);
                for cy in min.y..max.y {
                    let bottom = cy * step;
                    let local = IVec3::new(cx, cy, cz) - min;
                    if bottom as f32 + half <= mean + 0.5 {
                        // The cell's topmost ground voxel.
                        let p = IVec3::new(center.x, (bottom + step - 1).min(h), center.y);
                        let q = p.as_vec3();
                        let id = self.ground_block(
                            p,
                            h - p.y,
                            beach,
                            || value(q / 5.0, self.seed + 40),
                            || value(q / 4.0, self.seed + 42),
                        );
                        grid.set(local, id);
                    } else if bottom as f32 + half <= SEA_LEVEL as f32 + 1.0 {
                        grid.set(local, block::WATER);
                    }
                }
            }
        }
        if self.flat {
            return grid;
        }

        let (lo, hi) = (min * step, max * step);
        for cell in Self::tree_cells(lo.xz(), hi.xz()) {
            let Some((base, key)) = self.tree_in(cell) else {
                continue;
            };
            let (crown, blobs) = self.tree_shape(base, key);
            let set = |grid: &mut VoxelGrid, c: IVec3, id: VoxelId| {
                let l = c - min;
                if grid.contains(l) && grid.get(l) == AIR {
                    grid.set(l, id);
                }
            };
            if step <= 4 {
                // The trunk's middle, between its two columns.
                let column = (base.xz() + 1).div_euclid(IVec2::splat(step));
                for cy in (base.y + 1).div_euclid(step)..=(crown.y - 1).div_euclid(step) {
                    set(&mut grid, IVec3::new(column.x, cy, column.y), block::BARK[0]);
                }
            }
            for blob in blobs {
                let (center, radii) = blob;
                let lo = ((center - radii * 1.35) / step as f32).floor().as_ivec3();
                let hi = ((center + radii * 1.35) / step as f32).floor().as_ivec3();
                for cz in lo.z..=hi.z {
                    for cy in lo.y..=hi.y {
                        for cx in lo.x..=hi.x {
                            let c = IVec3::new(cx, cy, cz);
                            if let Some(leaves) = self.leaf_at((c * step).as_vec3() + half, blob) {
                                set(&mut grid, c, leaves);
                            }
                        }
                    }
                }
            }
        }
        grid
    }
}

fn get(grid: &VoxelGrid, min: IVec3, p: IVec3) -> VoxelId {
    grid.get(p - min)
}

fn set_if_air(grid: &mut VoxelGrid, min: IVec3, p: IVec3, id: VoxelId) {
    let l = p - min;
    if grid.contains(l) && matches!(grid.get(l), AIR | block::TUFT) {
        grid.set(l, id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The top ground cell of a coarse column, in coarse cells.
    fn top(grid: &VoxelGrid, x: i32, z: i32) -> Option<i32> {
        (0..grid.size().y as i32).rev().find(|y| is_ground(grid.get(IVec3::new(x, *y, z))))
    }

    #[test]
    fn coarse_ground_follows_the_fine_ground() {
        let terrain = Terrain { seed: 1337, flat: false };
        for step in [2, 4, 8] {
            let size = UVec3::new(16, (WORLD_HEIGHT / step) as u32, 16);
            let coarse = terrain.generate_coarse(IVec3::new(-8, 0, 3), size, step);
            let mut off = 0;
            for z in 0..16 {
                for x in 0..16 {
                    let (wx, wz) = ((x - 8) * step + step / 2, (z + 3) * step + step / 2);
                    let fine = terrain.height(wx, wz) / step;
                    let got = top(&coarse, x, z).expect("every column has ground");
                    off += ((got - fine).abs() > 1) as i32;
                }
            }
            // Steep slopes can differ by more than a cell between a column's mean and
            // its middle; most columns don't.
            assert!(off < 16, "step {step}: {off} columns off by more than a cell");
        }
    }

    #[test]
    fn coarse_terrain_has_water_and_trees() {
        let terrain = Terrain { seed: 1337, flat: false };
        let grid = terrain.generate_coarse(IVec3::new(-64, 0, -64), UVec3::new(128, 32, 128), 4);
        let count = |id: VoxelId| grid.cells().iter().filter(|c| **c == id).count();
        assert!(count(block::WATER) > 0);
        assert!(block::LEAVES.iter().map(|l| count(*l)).sum::<usize>() > 0);
        assert!(count(block::BARK[0]) > 0);
    }
}
