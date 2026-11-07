//! Deterministic terrain: every voxel is a pure function of its position and the seed,
//! so chunks generate independently (and in parallel) without seams.

use bevy::prelude::*;

use crate::voxel::{
    AIR, Palette, PaletteEntry, Surface, VoxelGrid, VoxelId,
    noise::{fbm, hash01, value},
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

#[derive(Clone, Copy)]
pub struct Terrain {
    pub seed: u32,
}

const TREE_CELL: i32 = 18;
/// Farthest a tree's leaves reach from its trunk, including noise.
const TREE_REACH: i32 = 14;

impl Terrain {
    /// Height of the top solid voxel in column `(x, z)`.
    pub fn height(&self, x: i32, z: i32) -> i32 {
        let p = Vec3::new(x as f32, 0.0, z as f32);
        let continent = fbm(p / 260.0, self.seed);
        let hills = fbm(p / 70.0, self.seed + 10);
        let ridges = 1.0 - (fbm(p / 110.0, self.seed + 20) * 2.0 - 1.0).abs();
        let mountain = ((continent - 0.45) * 2.5).clamp(0.0, 1.0);
        let h = 22.0
            + continent * 26.0
            + (hills - 0.5) * 22.0
            + ridges.powi(3) * mountain * 48.0
            + (value(p / 9.0, self.seed + 30) - 0.5) * 3.0;
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

        // Terrain columns.
        for z in min.z..max.z {
            for x in min.x..max.x {
                let h = height(x, z);
                let beach = h <= SEA_LEVEL + 1;
                // Below y = 0 is solid stone, so the world has no visible underside.
                for y in min.y..max.y.min(h + 1).max(min.y) {
                    let p = IVec3::new(x, y, z);
                    let depth = h - y;
                    let patch = value(p.as_vec3() / 5.0, self.seed + 40);
                    let id = if depth == 0 && h > 78 {
                        block::SNOW
                    } else if beach && depth < 4 {
                        pick(block::SAND, patch)
                    } else if depth == 0 {
                        pick(block::GRASS, fbm(p.as_vec3() / 12.0, self.seed + 41) * 1.4 - 0.2)
                    } else if depth < 4 {
                        pick(block::DIRT, patch)
                    } else {
                        pick(block::STONE, value(p.as_vec3() / 4.0, self.seed + 42))
                    };
                    grid.set(local(p), id);
                }
                for y in (h + 1).max(min.y)..=SEA_LEVEL.min(max.y - 1) {
                    grid.set(local(IVec3::new(x, y, z)), block::WATER);
                }
            }
        }

        // Trees, one candidate per jittered cell.
        let cells = |a: i32, b: i32| (a - TREE_REACH).div_euclid(TREE_CELL)..=(b + TREE_REACH).div_euclid(TREE_CELL);
        for cz in cells(min.z, max.z) {
            for cx in cells(min.x, max.x) {
                self.place_tree(&mut grid, min, IVec2::new(cx, cz));
            }
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

    fn place_tree(&self, grid: &mut VoxelGrid, min: IVec3, cell: IVec2) {
        let key = IVec3::new(cell.x, 0, cell.y);
        // Groves and open meadows rather than uniform forest.
        let grove = fbm(Vec3::new(cell.x as f32, 0.0, cell.y as f32) / 6.0, self.seed + 59);
        if hash01(key, self.seed + 60) > (grove - 0.35) * 1.4 {
            return;
        }
        let jitter = |k: u32| (hash01(key, self.seed + k) * (TREE_CELL - 6) as f32) as i32 + 3;
        let (x, z) = (cell.x * TREE_CELL + jitter(61), cell.y * TREE_CELL + jitter(62));
        let ground = self.height(x, z);
        let steep = (self.height(x + 2, z) - self.height(x - 2, z)).abs()
            + (self.height(x, z + 2) - self.height(x, z - 2)).abs();
        if ground <= SEA_LEVEL + 1 || ground > 70 || steep > 5 {
            return;
        }
        let grid_max = min + grid.size().as_ivec3();
        if ground + 1 >= grid_max.y || ground + 37 < min.y {
            return;
        }
        self.grow_tree(grid, min, IVec3::new(x, ground, z), key);
    }

    /// Grows a tree whose 2x2 trunk stands on `base` (the ground voxel), into the part
    /// of `grid` (covering world voxels from `min`) it reaches. `key` picks its variation.
    pub fn grow_tree(&self, grid: &mut VoxelGrid, min: IVec3, base: IVec3, key: IVec3) {
        let (x, ground, z) = (base.x, base.y, base.z);
        let trunk_height = 13 + (hash01(key, self.seed + 63) * 8.0) as i32;
        let crown = IVec3::new(x, ground + trunk_height, z);

        // 2x2 trunk.
        for y in ground + 1..crown.y {
            for (dx, dz) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let p = IVec3::new(x + dx, y, z + dz);
                set_if_air(grid, min, p, pick(block::BARK, hash01(p, 64)));
            }
        }

        // A couple of noisy leaf blobs around the crown.
        let radius = 5.5 + hash01(key, self.seed + 65) * 2.5;
        let blobs = [
            (crown.as_vec3() + Vec3::new(1.0, 0.0, 1.0), Vec3::new(radius, radius * 0.75, radius)),
            (crown.as_vec3() + Vec3::new(1.0, radius * 0.6, 1.0), Vec3::splat(radius * 0.65)),
        ];
        for (center, radii) in blobs {
            // Only visit the part of the blob inside this grid.
            let lo = (center - radii - 2.0).floor().as_ivec3().max(min);
            let hi = (center + radii + 2.0).ceil().as_ivec3().min(min + grid.size().as_ivec3() - 1);
            for pz in lo.z..=hi.z {
                for py in lo.y..=hi.y {
                    for px in lo.x..=hi.x {
                        let p = IVec3::new(px, py, pz);
                        let q = p.as_vec3() + 0.5;
                        let wobble = (fbm(q / 3.0, self.seed + 66) - 0.5) * 0.7;
                        if ((q - center) / radii).length() > 1.0 + wobble {
                            continue;
                        }
                        let shade = fbm(q / 4.0, self.seed + 67) + (q.y - center.y) / radii.y * 0.3;
                        set_if_air(grid, min, p, pick(block::LEAVES, 1.0 - shade));
                    }
                }
            }
        }
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
