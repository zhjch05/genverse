//! Deterministic terrain: every voxel is a pure function of its position and the seed,
//! so chunks generate independently (and in parallel) without seams.
//!
//! A world keeps the terrain version it was made with ([`Terrain::version`]): its edits
//! are voxels laid over that ground, so the ground under them never changes.

use bevy_color::prelude::*;
use bevy_math::prelude::*;

use crate::recipe::{Cover, Recipe};
use crate::voxel::{
    AIR, Palette, PaletteEntry, Surface, VoxelGrid, VoxelId,
    noise::{ValueGrid, fbm, fbm_xz, hash01, value, value_xz},
};

/// World voxels are 25 cm: fine enough to read as "HD", coarse enough to build with.
pub const VOXEL_SIZE: f32 = 0.25;
/// The world is this many voxels (128 m) tall; below is bedrock, above is sky.
pub const WORLD_HEIGHT: i32 = 512;
/// The terrain new worlds are made with: the continents, valleys and mountain ranges of
/// version 2 (up to about 100 m over a sea 16 m up), with more to see on them: rivers
/// through the lowlands, pines and bushes among the trees, boulders, flower meadows,
/// bare earth and pebble shores.
pub const TERRAIN_VERSION: u32 = 3;
/// The first terrain, for worlds made before there were versions: hills up to about
/// 18 m over a sea 7.5 m up, in a world 32 m tall.
pub const LEGACY_TERRAIN: u32 = 1;

pub fn legacy_terrain() -> u32 {
    LEGACY_TERRAIN
}

/// The sea's top voxel on the first terrain, and in flat worlds.
const LEGACY_SEA: i32 = 30;
/// The sea's top voxel since version 2.
const SEA: i32 = 64;

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
    /// The first voxel that isn't the ground's own: a world's own blocks start here
    /// ([`crate::content`]).
    pub const OWN: VoxelId = 22;
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

/// Whether this is ground (as opposed to water, plants, trees, or air). A world's own
/// blocks are.
pub fn is_ground(id: VoxelId) -> bool {
    block::GRASS.contains(&id)
        || block::DIRT.contains(&id)
        || block::STONE.contains(&id)
        || block::SAND.contains(&id)
        || id == block::SNOW
        || id >= block::OWN
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
    /// Which generator shapes the ground: [`TERRAIN_VERSION`] for new worlds.
    pub version: u32,
    /// The landscape the generator makes ([`Recipe`]): the usual one unless the world
    /// was made with its own.
    pub recipe: Recipe,
}

/// The ground's top voxel in a flat world: a little above where the sea would be.
pub const FLAT_GROUND: i32 = LEGACY_SEA + 4;

const TREE_CELL: i32 = 18;
/// Farthest a tree's leaves reach from its trunk, including noise.
const TREE_REACH: i32 = 14;
/// Boulders are placed one candidate per cell of this many voxels,
const ROCK_CELL: i32 = 26;
/// reach at most this far from their middle,
const ROCK_REACH: i32 = 6;
/// and rise at most this far over the ground.
const ROCK_RISE: i32 = 6;

/// The kinds of tree, since version 3. A natural tree's key says which, in its `y`.
const PINE: i32 = -1;
const BUSH: i32 = -2;

/// Column heights over a rectangle, with one more column (or coarse cell) around it,
/// for working out slopes at its edge.
pub struct Heights {
    /// The first column inside the rectangle.
    min: IVec2,
    /// The rectangle's size, without the border.
    size: IVec2,
    heights: Vec<f32>,
}

impl Heights {
    fn new(min: IVec2, size: IVec2, mut height: impl FnMut(IVec2) -> f32) -> Self {
        let heights = (min.y - 1..min.y + size.y + 1)
            .flat_map(|z| (min.x - 1..min.x + size.x + 1).map(move |x| IVec2::new(x, z)))
            .map(&mut height)
            .collect();
        Self { min, size, heights }
    }

    /// The height of column `c`, which is in the rectangle or its border.
    pub fn get(&self, c: IVec2) -> f32 {
        let l = c - self.min + 1;
        self.heights[(l.y * (self.size.x + 2) + l.x) as usize]
    }

    /// The highest column, border included.
    pub fn max(&self) -> f32 {
        self.heights.iter().copied().fold(f32::MIN, f32::max)
    }

    /// How much the ground climbs per column at `c`, along the steeper axis.
    fn slope(&self, c: IVec2) -> f32 {
        let dx = self.get(c + IVec2::X) - self.get(c - IVec2::X);
        let dz = self.get(c + IVec2::Y) - self.get(c - IVec2::Y);
        dx.abs().max(dz.abs()) / 2.0
    }
}

impl Terrain {
    /// Ground for a new world, on the current terrain.
    pub fn new(seed: u32, flat: bool) -> Self {
        Self { seed, flat, version: TERRAIN_VERSION, recipe: Recipe::NATURAL }
    }

    /// Ground for a new world with a landscape of its own.
    pub fn made(seed: u32, flat: bool, recipe: Recipe) -> Self {
        Self { recipe: recipe.bounded(), ..Self::new(seed, flat) }
    }

    /// The colors of this ground's blocks: the usual palette, with the recipe's.
    pub fn palette(&self) -> Palette {
        let mut palette = palette();
        self.recipe.recolor(&mut palette);
        palette
    }

    /// Whether the low ground is under water.
    fn wet(&self) -> bool {
        !self.flat && self.recipe.water
    }

    /// The sea's top voxel: open water fills up to it. Flat worlds have no sea, but
    /// heights are given relative to this, so theirs stays where it always was.
    pub fn sea_level(&self) -> i32 {
        if self.flat || self.version < 2 { LEGACY_SEA } else { SEA }
    }

    /// The highest the ground goes, natural or reshaped: room is left above it for
    /// trees.
    pub fn max_ground(&self) -> i32 {
        // The first terrain's world was 128 voxels tall.
        if self.version < 2 { 88 } else { WORLD_HEIGHT - 40 }
    }

    /// Whether this is the taller terrain of version 2 and up.
    fn tall(&self) -> bool {
        !self.flat && self.version >= 2
    }

    /// Whether this is the varied land of version 3 and up: rivers, more kinds of
    /// tree, boulders, meadows.
    fn lush(&self) -> bool {
        !self.flat && self.version >= 3
    }

    /// Height of the top solid voxel in column `(x, z)`.
    pub fn height(&self, x: i32, z: i32) -> i32 {
        if self.flat {
            return FLAT_GROUND;
        }
        let p = Vec2::new(x as f32, z as f32);
        if self.version < 2 {
            return self.legacy_height(p);
        }
        // The recipe's amounts as multiples of the usual, and its leanings as shifts:
        // the usual recipe multiplies by exactly 1 and adds exactly 0, so its ground is
        // the ground from before recipes, voxel for voxel.
        let r = self.recipe;
        let part = |percent: u16| percent as f32 / 100.0;
        let lean = |by: i8, most: f32| by as f32 / 100.0 * most;
        let wide = part(r.scale);
        // Coasts: the land rises out of a sea floor about 10 m down to lowlands a few
        // meters over the water, across continents a few hundred meters wide.
        let continent = fbm_xz(p / (1000.0 * wide), self.seed) + lean(r.land, 0.5);
        let land = smooth(0.30, 0.50, continent);
        let lowlands = SEA as f32 - 40.0 + land * 56.0;
        // Rolling hills and valleys, gentler near the sea.
        let hills = (fbm_xz(p / (220.0 * wide), self.seed + 10) - 0.5) * 48.0 * (0.3 + land) * part(r.hills);
        // Mountain ranges, where a broad noise is high inland: sharp ridges between
        // deep valleys, lifted as a whole too.
        let range = smooth(0.42, 0.62, fbm_xz(p / (760.0 * wide), self.seed + 5) + lean(r.ranges, 0.4)) * land;
        let ridge = 1.0 - (fbm_xz(p / (320.0 * wide), self.seed + 20) * 2.0 - 1.0).abs();
        let mountains = range * (40.0 + ridge.powi(3) * 300.0) * part(r.mountains);
        // Rocky detail, rougher in the mountains.
        let rock = (fbm_xz(p / 40.0, self.seed + 31) - 0.5) * (4.0 + range * 24.0) * part(r.roughness);
        let grain = (value_xz(p / 9.0, self.seed + 30) - 0.5) * 3.0 * part(r.roughness);
        let h = lowlands + hills + mountains + rock + grain;
        // Rivers (since version 3): winding channels cut down to just under the sea,
        // through the lowlands only, their banks easing up to the land beside them.
        let h = if self.version >= 3 && r.water {
            let bed = SEA as f32 - 5.0;
            let off = (fbm_xz(p / (420.0 * wide), self.seed + 70) - 0.5).abs();
            let channel = 1.0 - smooth(0.010, 0.040, off);
            let low = 1.0 - smooth(SEA as f32 + 24.0, SEA as f32 + 64.0, h);
            // Shallower where the recipe's land rolls less: level ground stays level.
            if h > bed { h + (bed - h) * channel * low * part(r.hills).min(1.0) } else { h }
        } else {
            h
        };
        // A recipe with higher mountains than the usual would be cut off flat at the top
        // of the world: its heights ease toward the top instead, so peaks stay peaks.
        let h = if r.is_default() { h } else { ease_under(h, self.max_ground() as f32) };
        (h.round() as i32).clamp(2, self.max_ground())
    }

    /// The first terrain's heights, as they always were.
    fn legacy_height(&self, p: Vec2) -> i32 {
        let continent = fbm_xz(p / 260.0, self.seed);
        let hills = fbm_xz(p / 70.0, self.seed + 10);
        let ridges = 1.0 - (fbm_xz(p / 110.0, self.seed + 20) * 2.0 - 1.0).abs();
        let mountain = ((continent - 0.45) * 2.5).clamp(0.0, 1.0);
        let h = 22.0
            + continent * 26.0
            + (hills - 0.5) * 22.0
            + ridges.powi(3) * mountain * 48.0
            + (value_xz(p / 9.0, self.seed + 30) - 0.5) * 3.0;
        (h.round() as i32).clamp(2, 104)
    }

    /// The heights of world columns `min .. min + size`, and a column around them.
    pub fn heights(&self, min: IVec2, size: IVec2) -> Heights {
        Heights::new(min, size, |c| self.height(c.x, c.y) as f32)
    }

    /// The mean heights of the columns under coarse cells `min .. min + size` of
    /// `step` world voxels a side, and a cell around them: sampled at up to 4x4 columns
    /// a cell.
    pub fn coarse_heights(&self, min: IVec2, size: IVec2, step: i32) -> Heights {
        let samples = step.min(4);
        let offsets: Vec<i32> = (0..samples).map(|i| (2 * i + 1) * step / (2 * samples)).collect();
        Heights::new(min, size, |c| {
            let mut sum = 0;
            for dz in &offsets {
                for dx in &offsets {
                    sum += self.height(c.x * step + dx, c.y * step + dz);
                }
            }
            sum as f32 / (samples * samples) as f32
        })
    }

    /// The highest voxel the seed fills in world columns `min..max`, whose highest
    /// ground is `ground`: that ground, or the sea, a plant, or a tree's crown over it.
    pub fn top(&self, min: IVec2, max: IVec2, ground: i32) -> i32 {
        if self.flat {
            return ground;
        }
        let mut top = if self.wet() { (ground + 1).max(self.sea_level()) } else { ground + 1 };
        if self.lush() {
            top = top.max(ground + ROCK_RISE);
        }
        for cell in Self::tree_cells(min, max) {
            if let Some((base, key)) = self.tree_in(cell) {
                let (_, blobs, _) = self.tree_shape(base, key);
                for (center, radii) in blobs {
                    top = top.max((center.y + radii.y * 1.35).ceil() as i32);
                }
            }
        }
        top
    }

    /// Fills a grid covering world voxels `min .. min + size`.
    pub fn generate(&self, min: IVec3, size: UVec3) -> VoxelGrid {
        let heights = self.heights(min.xz(), size.xz().as_ivec2());
        self.generate_in(&heights, min, size)
    }

    /// Like [`generate`](Self::generate), with the heights of its columns (from
    /// [`heights`](Self::heights)) already worked out.
    pub fn generate_in(&self, heights: &Heights, min: IVec3, size: UVec3) -> VoxelGrid {
        let mut grid = VoxelGrid::new(size);
        let max = min + size.as_ivec3();
        let local = |p: IVec3| p - min;
        let height = |x: i32, z: i32| heights.get(IVec2::new(x, z)) as i32;
        let sea = self.sea_level();

        // Terrain columns, with the noise that picks their blocks laid out over the part
        // of the grid they reach.
        let top = (heights.max() as i32 + 1).clamp(min.y, max.y);
        let reach = IVec3::new(size.x as i32, top - min.y, size.z as i32);
        if reach.y <= 0 && (min.y > sea || !self.wet()) {
            // All sky, unless a tree reaches up into it.
            return self.add_trees(grid, min, max, &height);
        }
        let patches = ValueGrid::new(min, reach, 5.0, self.seed + 40);
        let stones = ValueGrid::new(min, reach, 4.0, self.seed + 42);
        for z in min.z..max.z {
            for x in min.x..max.x {
                let h = height(x, z);
                let beach = h <= sea + 1 && self.wet();
                let steep = self.tall() && heights.slope(IVec2::new(x, z)) >= STEEP;
                // Below y = 0 is solid stone, so the world has no visible underside.
                for y in min.y..max.y.min(h + 1).max(min.y) {
                    let p = IVec3::new(x, y, z);
                    let depth = h - y;
                    let id = self.ground_block(p, depth, beach, steep, || patches.get(local(p)), || stones.get(local(p)));
                    grid.set(local(p), id);
                }
                if self.wet() {
                    for y in (h + 1).max(min.y)..=sea.min(max.y - 1) {
                        grid.set(local(IVec3::new(x, y, z)), block::WATER);
                    }
                }
            }
        }
        self.add_trees(grid, min, max, &height)
    }

    /// Trees, then tufts and flowers, in a grid of ground.
    fn add_trees(&self, mut grid: VoxelGrid, min: IVec3, max: IVec3, height: &impl Fn(i32, i32) -> i32) -> VoxelGrid {
        if self.flat {
            return grid;
        }
        let local = |p: IVec3| p - min;
        let lush = self.lush();
        // Boulders, half sunk in the ground (and the shallows).
        if lush {
            for cell in Self::rock_cells(min.xz(), max.xz()) {
                self.place_rock(&mut grid, min, cell);
            }
        }
        // Trees, one candidate per jittered cell.
        for cell in Self::tree_cells(min.xz(), max.xz()) {
            self.place_tree(&mut grid, min, cell);
        }
        let plants = self.recipe.plants as f32 / 100.0;
        let sea = self.sea_level();

        // Grass tufts and flowers on exposed grass.
        for z in min.z..max.z {
            for x in min.x..max.x {
                let h = height(x, z);
                if h + 1 < min.y || h >= max.y {
                    continue;
                }
                let top = IVec3::new(x, h, z);
                let above = top + IVec3::Y;
                if get(&grid, min, above) != AIR {
                    continue;
                }
                let on = get(&grid, min, top);
                // Reeds along the water's edge.
                if lush && self.wet() && h <= sea + 1 && block::SAND.contains(&on) {
                    if hash01(top, self.seed + 54) < 0.07 * plants.min(2.0) {
                        grid.set(local(above), block::TUFT);
                    }
                    continue;
                }
                if !block::GRASS.contains(&on) {
                    continue;
                }
                let roll = hash01(top, self.seed + 50);
                let (mut flowers, mut tufts) = (0.006, 0.03);
                // Meadows thick with flowers, and stretches of long grass, between
                // plainer ground. Only the columns that could grow something ask.
                if lush && roll < 0.23 * plants {
                    let q = top.xz().as_vec2();
                    flowers *= 1.0 + 12.0 * smooth(0.58, 0.72, fbm_xz(q / 28.0, self.seed + 52));
                    tufts = flowers + 0.024 * (1.0 + 5.0 * smooth(0.52, 0.68, fbm_xz(q / 20.0, self.seed + 53)));
                }
                if roll < flowers * plants {
                    let flower = pick(block::FLOWERS, value(top.as_vec3() / 20.0, self.seed + 51));
                    grid.set(local(above), flower);
                } else if roll < tufts * plants {
                    grid.set(local(above), block::TUFT);
                }
            }
        }
        grid
    }

    /// The ground block at `p`, `depth` voxels under the top of its column; `beach`
    /// when the column is at the shore and `steep` on a cliff. `patch` and `stone`
    /// give the noise that picks among sands and dirts, and among stones, at `p`.
    fn ground_block(
        &self,
        p: IVec3,
        depth: i32,
        beach: bool,
        steep: bool,
        patch: impl Fn() -> f32,
        stone: impl Fn() -> f32,
    ) -> VoxelId {
        let h = p.y + depth;
        if depth == 0 && h > self.snow_line(p) {
            block::SNOW
        } else if beach && depth < 4 {
            // Stretches of pebble shore between the sands.
            if self.lush() && value_xz(p.xz().as_vec2() / 22.0, self.seed + 45) > 0.66 {
                pick(block::STONE, stone())
            } else {
                pick(block::SAND, patch())
            }
        } else if steep && depth < 4 {
            pick(block::STONE, stone())
        } else if depth == 0 {
            match self.recipe.ground {
                // Patches of bare earth in the grass.
                Cover::Grass if self.lush() && fbm_xz(p.xz().as_vec2() / 16.0, self.seed + 44) > 0.70 => pick(block::DIRT, patch()),
                Cover::Grass => pick(block::GRASS, fbm(p.as_vec3() / 12.0, self.seed + 41) * 1.4 - 0.2),
                Cover::Sand => pick(block::SAND, patch()),
                Cover::Snow => block::SNOW,
                Cover::Stone => pick(block::STONE, stone()),
                Cover::Dirt => pick(block::DIRT, patch()),
            }
        } else if depth < 4 {
            match self.recipe.ground {
                Cover::Sand => pick(block::SAND, patch()),
                Cover::Stone => pick(block::STONE, stone()),
                _ => pick(block::DIRT, patch()),
            }
        } else {
            pick(block::STONE, stone())
        }
    }

    /// Ground over this is capped with snow, around `p`.
    fn snow_line(&self, p: IVec3) -> i32 {
        if self.version < 2 {
            return 78;
        }
        // The recipe brings it down to the sea, or lifts it over the highest ground.
        SEA + 230 - self.recipe.snow as i32 * 23 / 10 + ((value_xz(p.xz().as_vec2() / 30.0, self.seed + 43) - 0.5) * 32.0) as i32
    }

    /// Trees grow on ground up to this.
    fn tree_line(&self) -> i32 {
        if self.version < 2 { 70 } else { SEA + 170 }
    }

    /// The tree of tree cell `cell`, if it has one: the ground voxel its trunk stands on
    /// and the key that picks its variation.
    fn tree_in(&self, cell: IVec2) -> Option<(IVec3, IVec3)> {
        let key = IVec3::new(cell.x, 0, cell.y);
        // Groves and open meadows rather than uniform forest.
        let grove = fbm_xz(cell.as_vec2() / 6.0, self.seed + 59);
        // More trees fill the groves, then the meadows between them.
        let trees = self.recipe.trees as f32 / 100.0;
        let outside = hash01(key, self.seed + 60) > (grove - 0.35) * 1.4 * trees + (trees - 1.0).max(0.0) * 0.25;
        // The meadows aren't bare: a bush here and there.
        let stray = outside && self.lush() && hash01(key, self.seed + 72) < 0.10 * trees.min(1.5);
        if outside && !stray {
            return None;
        }
        let jitter = |k: u32| (hash01(key, self.seed + k) * (TREE_CELL - 6) as f32) as i32 + 3;
        let (x, z) = (cell.x * TREE_CELL + jitter(61), cell.y * TREE_CELL + jitter(62));
        let ground = self.height(x, z);
        let steep = (self.height(x + 2, z) - self.height(x - 2, z)).abs()
            + (self.height(x, z + 2) - self.height(x, z - 2)).abs();
        if (self.wet() && ground <= self.sea_level() + 1) || ground > self.tree_line() || steep > 5 {
            return None;
        }
        // Which kind (since version 3): pines take over as the ground climbs, and
        // stand in woods of their own lower down; bushes fill in under the others.
        let kind = if !self.lush() {
            0
        } else if stray {
            BUSH
        } else {
            let high = smooth(SEA as f32 + 30.0, SEA as f32 + 120.0, ground as f32);
            let cold = high + (fbm_xz(cell.as_vec2() / 5.0, self.seed + 68) - 0.5) * 1.2;
            if hash01(key, self.seed + 69) < cold {
                PINE
            } else if hash01(key, self.seed + 71) < 0.2 {
                BUSH
            } else {
                0
            }
        };
        Some((IVec3::new(x, ground, z), IVec3::new(key.x, kind, key.z)))
    }

    /// The rock cells whose boulders can reach world columns `min..max`.
    fn rock_cells(min: IVec2, max: IVec2) -> impl Iterator<Item = IVec2> {
        let cells = |a: i32, b: i32| (a - ROCK_REACH).div_euclid(ROCK_CELL)..=(b + ROCK_REACH).div_euclid(ROCK_CELL);
        cells(min.y, max.y).flat_map(move |cz| cells(min.x, max.x).map(move |cx| IVec2::new(cx, cz)))
    }

    /// The boulder of rock cell `cell`, if it has one: its middle and radii, in world
    /// voxels.
    fn rock_in(&self, cell: IVec2) -> Option<(Vec3, Vec3)> {
        let key = IVec3::new(cell.x, 1, cell.y);
        if hash01(key, self.seed + 80) > 0.22 {
            return None;
        }
        let jitter = |k: u32| (hash01(key, self.seed + k) * (ROCK_CELL - 12) as f32) as i32 + 6;
        let (x, z) = (cell.x * ROCK_CELL + jitter(81), cell.y * ROCK_CELL + jitter(82));
        let size = hash01(key, self.seed + 83);
        let r = 1.6 + size * size * 3.4;
        // It sits on the lowest ground under it, so no side hangs in the air; not on
        // cliffs, nor out in deep water.
        let d = r as i32;
        let grounds = [(0, 0), (d, 0), (-d, 0), (0, d), (0, -d)].map(|(dx, dz)| self.height(x + dx, z + dz));
        let (low, high) = (*grounds.iter().min().unwrap(), *grounds.iter().max().unwrap());
        if high - low > 4 || (self.wet() && low < self.sea_level() - 2) {
            return None;
        }
        let radii = Vec3::new(r, r * 0.75, r * (0.7 + 0.3 * hash01(key, self.seed + 84)));
        Some((Vec3::new(x as f32 + 0.5, low as f32 + r * 0.2, z as f32 + 0.5), radii))
    }

    fn place_rock(&self, grid: &mut VoxelGrid, min: IVec3, cell: IVec2) {
        let Some((center, radii)) = self.rock_in(cell) else {
            return;
        };
        let lo = (center - radii - 1.0).floor().as_ivec3().max(min);
        let hi = (center + radii + 1.0).ceil().as_ivec3().min(min + grid.size().as_ivec3() - 1);
        for pz in lo.z..=hi.z {
            for py in lo.y..=hi.y {
                for px in lo.x..=hi.x {
                    let p = IVec3::new(px, py, pz);
                    let q = p.as_vec3() + 0.5;
                    let lumps = (value(q / 2.5, self.seed + 85) - 0.5) * 0.3;
                    if ((q - center) / radii).length() <= 1.0 + lumps && matches!(get(grid, min, p), AIR | block::WATER) {
                        grid.set(p - min, pick(block::STONE, value(q / 4.0, self.seed + 42)));
                    }
                }
            }
        }
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
    /// its crown's leaf blobs as centers and radii, and how dark its leaves are.
    fn tree_shape(&self, base: IVec3, key: IVec3) -> (IVec3, [(Vec3, Vec3); 2], f32) {
        let middle = base.as_vec3() + Vec3::new(1.0, 0.0, 1.0);
        if self.lush() && key.y == PINE {
            // A tall trunk in a narrow cone of dark needles: a wide skirt, a spire over it.
            let height = 18.0 + (hash01(key, self.seed + 63) * 8.0).floor();
            let radius = 4.4 + hash01(key, self.seed + 65) * 1.4;
            let blobs = [
                (middle + Vec3::Y * height * 0.5, Vec3::new(radius, height * 0.3, radius)),
                (middle + Vec3::Y * height * 0.88, Vec3::new(radius * 0.55, height * 0.28, radius * 0.55)),
            ];
            return (base + IVec3::Y * height as i32, blobs, 0.45);
        }
        if self.lush() && key.y == BUSH {
            // Leaves nearly down to the ground, and lighter than a tree's.
            let radius = 2.6 + hash01(key, self.seed + 65) * 1.6;
            let lean = (hash01(key, self.seed + 63) - 0.5) * radius;
            let blobs = [
                (middle + Vec3::Y * 3.0, Vec3::new(radius, radius * 0.7, radius)),
                (middle + Vec3::new(lean, 3.0 + radius * 0.35, -lean), Vec3::splat(radius * 0.6)),
            ];
            return (base + IVec3::Y * 2, blobs, -0.15);
        }
        let trunk_height = 13 + (hash01(key, self.seed + 63) * 8.0) as i32;
        let crown = IVec3::new(base.x, base.y + trunk_height, base.z);
        let radius = 5.5 + hash01(key, self.seed + 65) * 2.5;
        let blobs = [
            (crown.as_vec3() + Vec3::new(1.0, 0.0, 1.0), Vec3::new(radius, radius * 0.75, radius)),
            (crown.as_vec3() + Vec3::new(1.0, radius * 0.6, 1.0), Vec3::splat(radius * 0.65)),
        ];
        (crown, blobs, 0.0)
    }

    /// Whether the point `q` (in world voxels) is inside a leaf blob, and if so the
    /// leaves there: darker with a higher `tone`.
    fn leaf_at(&self, q: Vec3, (center, radii): (Vec3, Vec3), tone: f32) -> Option<VoxelId> {
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
        Some(pick(block::LEAVES, 1.0 - shade + tone))
    }

    /// Grows a tree whose 2x2 trunk stands on `base` (the ground voxel), into the part
    /// of `grid` (covering world voxels from `min`) it reaches. `key` picks its variation.
    pub fn grow_tree(&self, grid: &mut VoxelGrid, min: IVec3, base: IVec3, key: IVec3) {
        let (x, ground, z) = (base.x, base.y, base.z);
        let (crown, blobs, tone) = self.tree_shape(base, key);

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
                        if let Some(leaves) = self.leaf_at(p.as_vec3() + 0.5, blob, tone) {
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
        let heights = self.coarse_heights(min.xz(), size.xz().as_ivec2(), step);
        self.generate_coarse_in(&heights, min, size, step)
    }

    /// Like [`generate_coarse`](Self::generate_coarse), with its cells' mean heights
    /// (from [`coarse_heights`](Self::coarse_heights)) already worked out.
    pub fn generate_coarse_in(&self, heights: &Heights, min: IVec3, size: UVec3, step: i32) -> VoxelGrid {
        let mut grid = VoxelGrid::new(size);
        let max = min + size.as_ivec3();
        let half = step as f32 / 2.0;
        let sea = self.sea_level();
        for cz in min.z..max.z {
            for cx in min.x..max.x {
                let mean = heights.get(IVec2::new(cx, cz));
                let h = mean.round() as i32;
                let beach = h <= sea + 1 && self.wet();
                let steep = self.tall() && heights.slope(IVec2::new(cx, cz)) / step as f32 >= STEEP;
                let center = IVec2::new(cx * step + step / 2, cz * step + step / 2);
                for cy in min.y..max.y {
                    let bottom = cy * step;
                    let local = IVec3::new(cx, cy, cz) - min;
                    if bottom as f32 + half <= mean + 0.5 {
                        // The cell's topmost ground voxel. On a landscape of its own, the
                        // column's top cell shows the surface (snow, sand), not what's
                        // under it; the usual one is left as it always looked from afar.
                        let surface = !self.recipe.is_default() && (bottom + step) as f32 + half > mean + 0.5;
                        let p = IVec3::new(center.x, if surface { h } else { (bottom + step - 1).min(h) }, center.y);
                        let q = p.as_vec3();
                        let id = self.ground_block(
                            p,
                            h - p.y,
                            beach,
                            steep,
                            || value(q / 5.0, self.seed + 40),
                            || value(q / 4.0, self.seed + 42),
                        );
                        grid.set(local, id);
                    } else if self.wet() && bottom as f32 + half <= sea as f32 + 1.0 {
                        grid.set(local, block::WATER);
                    } else {
                        break;
                    }
                }
            }
        }
        if self.flat {
            return grid;
        }

        let (lo, hi) = (min * step, max * step);
        // Boulders, while they're still as big as a cell.
        if self.lush() && step <= 4 {
            for cell in Self::rock_cells(lo.xz(), hi.xz()) {
                let Some((center, radii)) = self.rock_in(cell) else {
                    continue;
                };
                let lo = ((center - radii) / step as f32).floor().as_ivec3().max(min);
                let hi = ((center + radii) / step as f32).floor().as_ivec3().min(max - 1);
                for cz in lo.z..=hi.z {
                    for cy in lo.y..=hi.y {
                        for cx in lo.x..=hi.x {
                            let c = IVec3::new(cx, cy, cz);
                            let q = (c * step).as_vec3() + half;
                            if ((q - center) / radii).length() <= 1.0 && matches!(grid.get(c - min), AIR | block::WATER) {
                                grid.set(c - min, block::STONE[1]);
                            }
                        }
                    }
                }
            }
        }
        for cell in Self::tree_cells(lo.xz(), hi.xz()) {
            let Some((base, key)) = self.tree_in(cell) else {
                continue;
            };
            let (crown, blobs, tone) = self.tree_shape(base, key);
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
                            if let Some(leaves) = self.leaf_at((c * step).as_vec3() + half, blob, tone) {
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

/// Ground climbing this many voxels per column is bare rock.
const STEEP: f32 = 1.6;

/// `h` as it is up to a knee 30 m under `top`, and easing toward `top` above it without
/// ever reaching it.
fn ease_under(h: f32, top: f32) -> f32 {
    let room = 120.0;
    let knee = top - room;
    if h <= knee {
        return h;
    }
    // Twice as far over the knee is always higher, less and less: ridges keep their shape.
    let over = (h - knee) / room;
    knee + room * over / (1.0 + over)
}

/// 0 below `a`, 1 above `b`, and smoothly between.
fn smooth(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
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
        let terrain = Terrain::new(1337, false);
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

    /// Worlds made before recipes keep exactly the ground they were made on: the usual
    /// recipe is that generator, voxel for voxel. The fingerprint is from before recipes.
    #[test]
    fn the_usual_recipe_is_the_terrain_from_before_recipes() {
        let t = Terrain { version: 2, ..Terrain::new(1337, false) };
        let mut h: u64 = 0xcbf29ce484222325;
        let mut eat = |b: u8| h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        let regions = [
            (IVec3::new(-40, 40, -40), UVec3::new(80, 200, 80)),
            (IVec3::new(900, 0, -2300), UVec3::new(48, 400, 48)),
            (IVec3::new(-5000, 60, 7000), UVec3::new(40, 300, 40)),
        ];
        for (min, size) in regions {
            for c in t.generate(min, size).cells() {
                eat(*c);
            }
        }
        for step in [2, 4, 8] {
            for c in t.generate_coarse(IVec3::new(-20, 0, 7), UVec3::new(34, (WORLD_HEIGHT / step) as u32, 34), step).cells() {
                eat(*c);
            }
        }
        for c in Terrain::new(7, true).generate(IVec3::new(-10, 0, -10), UVec3::new(20, 64, 20)).cells() {
            eat(*c);
        }
        assert_eq!(t.top(IVec2::new(-64, -64), IVec2::new(64, 64), 80), USUAL_TOP);
        assert_eq!(h, USUAL_PRINT);
    }
    const USUAL_PRINT: u64 = 4852745813946194973;

    /// How much of a region is each kind of block.
    fn share(terrain: Terrain, of: impl Fn(VoxelId) -> bool) -> f32 {
        let grid = terrain.generate(IVec3::new(-96, 40, -96), UVec3::new(192, 160, 192));
        let cells = grid.cells();
        cells.iter().filter(|c| of(**c)).count() as f32 / cells.len() as f32
    }

    #[test]
    fn recipes_change_the_landscape_they_say_they_do() {
        let usual = Terrain::new(1337, false);
        let made = |name: &str| Terrain::made(1337, false, Recipe::named(name).unwrap());
        let water = |t: Terrain| share(t, |c| c == block::WATER);
        let leaves = |t: Terrain| share(t, |c| block::LEAVES.contains(&c));
        let grass = |t: Terrain| share(t, |c| block::GRASS.contains(&c));
        // No sea in a desert, nor grass, nor trees; sand instead.
        let desert = made("desert");
        assert_eq!((water(desert), grass(desert), leaves(desert)), (0.0, 0.0, 0.0));
        assert!(share(desert, |c| block::SAND.contains(&c)) > share(usual, |c| block::SAND.contains(&c)));
        // Islands are mostly sea; a forest has many more leaves; tundra is under snow.
        assert!(water(made("islands")) > water(usual));
        assert!(leaves(made("forest")) > leaves(usual) * 1.5, "{} vs {}", leaves(made("forest")), leaves(usual));
        assert!(share(made("tundra"), |c| c == block::SNOW) > 0.0 && grass(made("tundra")) == 0.0);
        // Dry low ground: no water anywhere, and no beaches for it.
        let dry = Terrain::made(1337, false, Recipe { water: false, ..Recipe::NATURAL });
        assert_eq!(water(dry), 0.0);
        // Level ground: every column at about the same height.
        let level = Terrain::made(1337, false, Recipe { land: 100, hills: 0, mountains: 0, roughness: 0, ..Recipe::NATURAL });
        let heights: Vec<i32> = (-50..50).map(|x| level.height(x * 7, x * 3)).collect();
        assert!(heights.iter().max().unwrap() - heights.iter().min().unwrap() <= 1, "{heights:?}");
        // Much taller mountains don't flatten off at the top of the world: no wide
        // level plateau at the highest ground.
        let giant = Terrain::made(1337, false, Recipe { land: 100, mountains: 300, ranges: 100, ..Recipe::NATURAL });
        let tops: Vec<i32> = (-80..80).flat_map(|x| (-80..80).map(move |z| (x, z))).map(|(x, z)| giant.height(x * 11, z * 11)).collect();
        let highest = *tops.iter().max().unwrap();
        assert!(highest < giant.max_ground(), "peaks ease under the top of the world");
        assert!(tops.iter().filter(|h| **h == highest).count() < tops.len() / 100, "the highest ground isn't a plateau");
        // From afar, a snowy land's tops are snow, not the dirt under it.
        let snowy = made("tundra");
        let far = snowy.generate_coarse(IVec3::new(-16, 0, -16), UVec3::new(32, (WORLD_HEIGHT / 8) as u32, 32), 8);
        let top_cells: Vec<VoxelId> = (0..32).flat_map(|x| (0..32).map(move |z| (x, z))).filter_map(|(x, z)| top(&far, x, z).map(|y| far.get(IVec3::new(x, y, z)))).collect();
        assert!(!top_cells.is_empty() && top_cells.iter().all(|c| !block::DIRT.contains(c)), "no dirt shows through the snow from afar");
        // Taller mountains reach higher than the usual ones somewhere.
        let peak = |t: Terrain| (-60..60).flat_map(|x| (-60..60).map(move |z| (x, z))).map(|(x, z)| t.height(x * 40, z * 40)).max().unwrap();
        assert!(peak(made("mountains")) > peak(usual));
        // A flat world keeps its level ground, with the recipe's cover.
        let sands = Terrain::made(7, true, Recipe { ground: Cover::Sand, ..Recipe::NATURAL });
        assert_eq!(sands.height(5, 5), FLAT_GROUND);
        assert!(block::SAND.contains(&sands.generate(IVec3::new(0, FLAT_GROUND, 0), UVec3::ONE).get(IVec3::ZERO)));
        // The coarse terrain far away agrees about water.
        let far = dry.generate_coarse(IVec3::new(-16, 0, -16), UVec3::new(32, (WORLD_HEIGHT / 8) as u32, 32), 8);
        assert!(!far.cells().contains(&block::WATER));
    }
    const USUAL_TOP: i32 = 81;

    /// Worlds on the first terrain keep exactly the ground they were made on: their
    /// edits are laid over it. The fingerprint is of the generator before versions.
    #[test]
    fn legacy_terrain_is_unchanged() {
        let t = Terrain { version: LEGACY_TERRAIN, ..Terrain::new(1337, false) };
        let mut h: u64 = 0xcbf29ce484222325;
        let mut eat = |b: u8| h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        for (min, size) in [(IVec3::new(-40, 0, -40), UVec3::new(80, 128, 80)), (IVec3::new(300, 20, -500), UVec3::new(34, 34, 34))] {
            for c in t.generate(min, size).cells() {
                eat(*c);
            }
        }
        for step in [2, 4, 8] {
            for c in t.generate_coarse(IVec3::new(-20, 0, 7), UVec3::new(34, (128 / step) as u32, 34), step).cells() {
                eat(*c);
            }
        }
        let flat = Terrain { version: LEGACY_TERRAIN, ..Terrain::new(7, true) };
        for c in flat.generate(IVec3::new(-10, 0, -10), UVec3::new(20, 64, 20)).cells() {
            eat(*c);
        }
        assert_eq!(h, 0x189374ed716d187c);
        // Above the first world's top, there's only sky.
        assert!(t.generate(IVec3::new(-40, 128, -40), UVec3::new(80, 64, 80)).cells().iter().all(|c| *c == AIR));
    }

    /// The new terrain uses the taller world: real mountains, a sea, and lowlands.
    #[test]
    fn terrain_has_mountains_and_sea() {
        let t = Terrain::new(1337, false);
        let heights: Vec<i32> = (-60..60).flat_map(|z| (-60..60).map(move |x| (x * 40, z * 40))).map(|(x, z)| t.height(x, z)).collect();
        let share = |f: &dyn Fn(i32) -> bool| heights.iter().filter(|h| f(**h)).count() as f32 / heights.len() as f32;
        assert!(heights.iter().max().unwrap() - t.sea_level() > 300, "peaks over 75 m");
        assert!(*heights.iter().max().unwrap() <= t.max_ground());
        let sea = share(&|h| h < t.sea_level());
        assert!((0.05..0.4).contains(&sea), "{sea} under the sea");
        let lowland = share(&|h| h >= t.sea_level() && h < t.sea_level() + 80);
        assert!(lowland > 0.3, "{lowland} lowland");
    }

    /// `top` bounds everything the seed fills, trees and water included.
    #[test]
    fn nothing_grows_above_the_top() {
        for t in [Terrain::new(1337, false), Terrain { version: LEGACY_TERRAIN, ..Terrain::new(1337, false) }, Terrain::new(3, true)] {
            for (x, z) in [(0, 0), (-700, 260), (1500, -900), (37, 4100)] {
                let (min, size) = (IVec2::new(x, z), IVec2::new(48, 48));
                let heights = t.heights(min, size);
                let top = t.top(min, min + size, heights.max() as i32);
                let grid = t.generate(IVec3::new(x, 0, z), UVec3::new(48, WORLD_HEIGHT as u32, 48));
                let highest = (0..WORLD_HEIGHT).rev().find(|y| {
                    (0..48).any(|dz| (0..48).any(|dx| grid.get(IVec3::new(dx, *y, dz)) != AIR))
                });
                assert!(highest.unwrap() <= top, "{t:?} at {x}, {z}: {highest:?} over {top}");
            }
        }
    }

    /// Version 3 lays more on the ground of version 2: rivers, boulders, and three
    /// kinds of tree.
    #[test]
    fn new_terrain_has_rivers_rocks_and_kinds_of_tree() {
        let t = Terrain::new(1337, false);
        let before = Terrain { version: 2, ..t };
        let columns = || (-150..150).flat_map(|z| (-150..150).map(move |x| (x * 16, z * 16)));
        // Rivers only ever lower the ground, and only some of it.
        let cut = columns().filter(|(x, z)| t.height(*x, *z) < before.height(*x, *z)).count();
        assert!(columns().all(|(x, z)| t.height(x, z) <= before.height(x, z)));
        assert!((500..30000).contains(&cut), "{cut} of 90000 columns in a river");
        let cells = || (-60..60).flat_map(|z| (-60..60).map(move |x| IVec2::new(x, z)));
        let kinds = |kind: i32| cells().filter_map(|c| t.tree_in(c)).filter(|(_, key)| key.y == kind).count();
        assert!(kinds(0) > 100 && kinds(PINE) > 100 && kinds(BUSH) > 100, "{} {} {}", kinds(0), kinds(PINE), kinds(BUSH));
        assert!(cells().filter_map(|c| before.tree_in(c)).all(|(_, key)| key.y == 0));
        // Boulders stand clear of the ground they sit on, and no higher or wider than
        // `top` and the cells that look for them allow.
        let rocks: Vec<(Vec3, Vec3)> = cells().filter_map(|c| t.rock_in(c)).collect();
        assert!(rocks.len() > 500, "{} boulders", rocks.len());
        assert!(rocks.iter().all(|(_, r)| r.y * 1.15 + r.x * 0.2 <= ROCK_RISE as f32 && r.x * 1.15 <= ROCK_REACH as f32));
        let stands = |(center, _): &(Vec3, Vec3)| {
            let c = center.floor().as_ivec3();
            let above = IVec3::new(c.x, t.height(c.x, c.z) + 1, c.z);
            block::STONE.contains(&t.generate(above, UVec3::ONE).get(IVec3::ZERO))
        };
        let big: Vec<_> = rocks.iter().filter(|(_, r)| r.x > 3.0).take(40).collect();
        assert!(big.iter().filter(|rock| stands(rock)).count() > 30);
    }

    #[test]
    fn coarse_terrain_has_water_and_trees() {
        let terrain = Terrain::new(1337, false);
        let grid = terrain.generate_coarse(IVec3::new(-64, 0, -64), UVec3::new(128, 32, 128), 4);
        let count = |id: VoxelId| grid.cells().iter().filter(|c| **c == id).count();
        assert!(count(block::WATER) > 0);
        assert!(block::LEAVES.iter().map(|l| count(*l)).sum::<usize>() > 0);
        assert!(count(block::BARK[0]) > 0);
    }
}

