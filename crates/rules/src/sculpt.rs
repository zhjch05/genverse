//! Terrain changes as data: a short list of high-level operations (raise, flatten,
//! water, paint, scatter, path, ...) inside a selected area, compiled into voxel edits.
//!
//! People (through an agent) say *what* should change; this turns it into voxels the
//! same way every time and checks it against limits, like `spec::compile` does for
//! builds. Nothing here trusts its input.
//!
//! Frames: an [`Area`] is a rectangle of world voxel columns. Operations use meters in
//! the area's frame: x and z run from its min corner (world +X and +Z). Levels are
//! meters relative to sea level (the sea surface is 0).

use bevy_math::prelude::*;
use serde::{Deserialize, Serialize};

use crate::content::Content;
use crate::terrain::{Terrain, VOXEL_SIZE, WORLD_HEIGHT, block, is_ground, pick};
use crate::voxel::{
    AIR, VoxelGrid, VoxelId,
    noise::{hash01, value},
};

/// Limits one terrain change must pass. Each server sets its own
/// ([`crate::settings::WorldRules`]).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerrainLimits {
    /// Longest side of a selected area, in meters.
    pub max_area_m: f32,
    pub max_ops: usize,
    /// Largest raise or lower, and deepest water, in meters.
    pub max_height_change_m: f32,
    pub max_path_points: usize,
    pub max_trees: usize,
    pub max_changed_voxels: usize,
}

impl Default for TerrainLimits {
    fn default() -> Self {
        Self {
            max_area_m: 48.0,
            max_ops: 24,
            max_height_change_m: 8.0,
            max_path_points: 32,
            max_trees: 60,
            max_changed_voxels: 600_000,
        }
    }
}

pub const OP_KINDS: [&str; 9] = ["raise", "lower", "flatten", "smooth", "terrace", "water", "paint", "scatter", "path"];
pub const CLEAR: &str = "clear";
pub const MATERIALS: [&str; 5] = ["grass", "dirt", "stone", "sand", "snow"];
pub const ITEMS: [&str; 6] = ["tree", "bush", "rock", "flowers", "grass", "plants"];

/// Columns read around the area, so trees and rocks near its edge aren't cut off.
const MARGIN: i32 = 12;
const H: usize = WORLD_HEIGHT as usize;

/// An axis-aligned rectangle of terrain columns, in world voxels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Area {
    pub min: [i32; 2],
    pub size: [i32; 2],
}

impl Area {
    pub fn from_corners(a: IVec2, b: IVec2) -> Self {
        let min = a.min(b);
        let size = (a.max(b) - min) + 1;
        Self { min: min.to_array(), size: size.to_array() }
    }

    pub fn min(&self) -> IVec2 {
        IVec2::from_array(self.min)
    }

    pub fn size(&self) -> IVec2 {
        IVec2::from_array(self.size)
    }

    pub fn size_m(&self) -> Vec2 {
        self.size().as_vec2() * VOXEL_SIZE
    }

    pub fn contains(&self, column: IVec2) -> bool {
        let local = column - self.min();
        local.cmpge(IVec2::ZERO).all() && local.cmplt(self.size()).all()
    }

    /// The center of column `(x, z)` in the area frame, in meters.
    pub fn to_local(self, column: IVec2) -> Vec2 {
        ((column - self.min()).as_vec2() + 0.5) * VOXEL_SIZE
    }

    /// The world column under an area-frame point.
    pub fn to_world(self, local: Vec2) -> IVec2 {
        self.min() + (local / VOXEL_SIZE).floor().as_ivec2()
    }
}

/// Ground level in meters relative to the sea surface of `terrain`, for ground voxel `y`.
pub fn level_of(terrain: Terrain, y: i32) -> f32 {
    (y - terrain.sea_level()) as f32 * VOXEL_SIZE
}

fn voxel_of_level(terrain: Terrain, level: f32) -> i32 {
    terrain.sea_level() + (level / VOXEL_SIZE).round() as i32
}

/// The ground voxel at the top of a column (`0` if there is none), the top of the
/// water above it (equal to the ground if dry), and what the ground is made of.
pub fn surface(column: &[VoxelId]) -> (i32, i32, VoxelId) {
    let Some(ground) = (0..column.len()).rev().find(|&y| is_ground(column[y])) else {
        return (0, 0, AIR);
    };
    let water = (ground + 1..column.len()).take_while(|&y| column[y] == block::WATER).last().unwrap_or(ground);
    (ground as i32, water as i32, column[ground])
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TerrainSpec {
    pub name: String,
    pub ops: Vec<TerrainOp>,
    /// What it is and what it's for, in a line ([`crate::semantic`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
}

/// One operation. Every field is present (strict structured output); unused ones are null.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TerrainOp {
    pub kind: String,
    /// Where it applies; null means the whole selected area.
    pub area: Option<OpArea>,
    /// `[x, z]` points of a path.
    pub points: Option<Vec<Vec<f64>>>,
    /// Meters to raise or lower by, or the terrace step.
    pub amount_m: Option<f64>,
    /// Target level for flatten and water, in meters relative to sea level.
    pub level_m: Option<f64>,
    /// Water depth below its level.
    pub depth_m: Option<f64>,
    /// Path width.
    pub width_m: Option<f64>,
    /// Soft edge: the change fades out over this distance inside the shape.
    pub edge_m: Option<f64>,
    pub material: Option<String>,
    pub item: Option<String>,
    /// 0..1: how much (scatter amount, paint coverage, smooth strength).
    pub density: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OpArea {
    /// "rect" or "ellipse" (inscribed in the rect).
    pub shape: String,
    pub x: f64,
    pub z: f64,
    pub w: f64,
    pub d: f64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SculptStats {
    pub changed: usize,
    /// Most the ground rose and fell, in meters.
    pub raised: f32,
    pub lowered: f32,
    pub trees: usize,
    /// Columns that gained water.
    pub water_columns: usize,
}

pub struct TerrainEdit {
    /// New voxels, in world voxel coordinates.
    pub edits: Vec<(IVec3, VoxelId)>,
    pub stats: SculptStats,
}

/// A copy of the terrain columns under an area (plus a margin), edited in place.
#[derive(Clone)]
pub struct Columns {
    min: IVec2,
    size: IVec2,
    vox: Vec<VoxelId>,
    orig: Vec<VoxelId>,
    ground: Vec<i32>,
    water: Vec<i32>,
    /// The highest the ground may be moved to ([`Terrain::max_ground`]).
    cap: i32,
}

impl Columns {
    /// The world columns [`read`](Self::read) asks for: `area` plus a margin, as
    /// `(min, size)`.
    pub fn bounds(area: Area) -> (IVec2, IVec2) {
        (area.min() - MARGIN, area.size() + 2 * MARGIN)
    }

    /// Reads the columns under `area`. `read(x, z, out)` fills one world column
    /// (`y = 0..WORLD_HEIGHT`) and returns false if it isn't loaded.
    pub fn read(area: Area, mut read: impl FnMut(i32, i32, &mut [VoxelId]) -> bool) -> Option<Self> {
        let (min, size) = Self::bounds(area);
        let n = (size.x * size.y) as usize;
        let mut vox = vec![AIR; n * H];
        for i in 0..n {
            let c = min + IVec2::new(i as i32 % size.x, i as i32 / size.x);
            // The margin may be unloaded; it's only written to if it is.
            if !read(c.x, c.y, &mut vox[i * H..(i + 1) * H]) && area.contains(c) {
                return None;
            }
        }
        let mut columns = Self { min, size, orig: vox.clone(), vox, ground: vec![0; n], water: vec![0; n], cap: WORLD_HEIGHT - 40 };
        for i in 0..n {
            columns.rescan(i);
        }
        Some(columns)
    }

    fn len(&self) -> usize {
        self.ground.len()
    }

    /// The voxel at `p` as it was read, before any edit; `None` outside the columns.
    pub fn original(&self, p: IVec3) -> Option<VoxelId> {
        let i = self.index(IVec2::new(p.x, p.z))?;
        (0..H as i32).contains(&p.y).then(|| self.orig[i * H + p.y as usize])
    }

    fn column_of(&self, i: usize) -> IVec2 {
        self.min + IVec2::new(i as i32 % self.size.x, i as i32 / self.size.x)
    }

    fn index(&self, c: IVec2) -> Option<usize> {
        let l = c - self.min;
        (l.cmpge(IVec2::ZERO).all() && l.cmplt(self.size).all()).then(|| (l.x + l.y * self.size.x) as usize)
    }

    fn col(&mut self, i: usize) -> &mut [VoxelId] {
        &mut self.vox[i * H..(i + 1) * H]
    }

    fn get(&self, p: IVec3) -> VoxelId {
        match self.index(p.xz()) {
            Some(i) if (0..H as i32).contains(&p.y) => self.vox[i * H + p.y as usize],
            _ => AIR,
        }
    }

    fn set(&mut self, p: IVec3, id: VoxelId) {
        if let Some(i) = self.index(p.xz())
            && (1..H as i32).contains(&p.y)
            && self.orig[i * H] != AIR
        {
            self.vox[i * H + p.y as usize] = id;
        }
    }

    fn rescan(&mut self, i: usize) {
        let (ground, water, _) = surface(&self.vox[i * H..(i + 1) * H]);
        self.ground[i] = ground;
        self.water[i] = water;
    }

    /// Ground voxel and its block for a world column, if it was read.
    #[cfg(test)]
    fn ground_at(&self, c: IVec2) -> Option<(i32, i32, VoxelId)> {
        let i = self.index(c)?;
        (self.orig[i * H] != AIR).then(|| (self.ground[i], self.water[i], self.vox[i * H + self.ground[i] as usize]))
    }

    /// Moves a column's ground to `new`: fills with its own soil when it rises, cuts
    /// when it falls. Whatever stands on it (plants, trees) moves with it. Water keeps
    /// its level; dry land stays dry.
    fn set_ground(&mut self, i: usize, new: i32) {
        let new = new.clamp(2, self.cap);
        let (g, water) = (self.ground[i], self.water[i]);
        if new == g || self.orig[i * H] == AIR {
            return;
        }
        let c = self.column_of(i);
        let col = self.col(i);
        let top = col[g as usize];
        let sub = subsoil(top);
        let above: Vec<(i32, VoxelId)> = (g + 1..H as i32)
            .filter_map(|y| {
                let id = col[y as usize];
                (id != AIR && id != block::WATER).then_some((y, id))
            })
            .collect();
        col[(g.min(new) + 1) as usize..].fill(AIR);
        let stone = |y: i32| pick(block::STONE, hash01(IVec3::new(c.x, y, c.y), 42));
        if new > g {
            for y in g..=new {
                col[y as usize] = match new - y {
                    0 => top,
                    1..=3 => sub,
                    _ => stone(y),
                };
            }
        } else {
            col[new as usize] = top;
            for y in (new - 3).max(1)..new {
                if col[y as usize] != AIR {
                    col[y as usize] = sub;
                }
            }
        }
        for (y, id) in above {
            let y = y + new - g;
            if y > new && y < H as i32 {
                col[y as usize] = id;
            }
        }
        let wet = water > g && water > new;
        if wet {
            for y in new + 1..=water {
                if col[y as usize] == AIR {
                    col[y as usize] = block::WATER;
                }
            }
        }
        self.ground[i] = new;
        self.water[i] = if wet { water } else { new };
    }

    /// Removes plants on a column, and returns the trunk voxels standing on it.
    fn clear_plants(&mut self, i: usize) -> Vec<IVec3> {
        let c = self.column_of(i);
        let g = self.ground[i];
        let col = self.col(i);
        let mut trunks = Vec::new();
        for y in g + 1..H as i32 {
            let id = col[y as usize];
            if id == block::TUFT || block::FLOWERS.contains(&id) {
                col[y as usize] = AIR;
            } else if block::BARK.contains(&id) {
                trunks.push(IVec3::new(c.x, y, c.y));
            }
        }
        trunks
    }

    /// Removes whole trees: every trunk voxel connected to `seeds`, and the leaves
    /// around those trunks.
    fn fell(&mut self, seeds: Vec<IVec3>) {
        if seeds.is_empty() {
            return;
        }
        const NEIGHBORS: [IVec3; 6] = [IVec3::X, IVec3::NEG_X, IVec3::Y, IVec3::NEG_Y, IVec3::Z, IVec3::NEG_Z];
        let mut seen = std::collections::HashSet::new();
        let mut stack = seeds;
        let mut trunk = Vec::new();
        while let Some(p) = stack.pop() {
            if trunk.len() > 4_000 || !block::BARK.contains(&self.get(p)) || !seen.insert(p) {
                continue;
            }
            trunk.push(p);
            stack.extend(NEIGHBORS.map(|n| p + n));
        }
        let near_trunk = |p: IVec3, trunk: &[IVec3]| trunk.iter().any(|t| (t.xz() - p.xz()).abs().max_element() <= 11 && p.y >= t.y);
        let mut stack: Vec<IVec3> = trunk.iter().flat_map(|p| NEIGHBORS.map(|n| *p + n)).collect();
        let mut leaves = Vec::new();
        while let Some(p) = stack.pop() {
            if leaves.len() > 40_000 || !block::LEAVES.contains(&self.get(p)) || !seen.insert(p) || !near_trunk(p, &trunk) {
                continue;
            }
            leaves.push(p);
            stack.extend(NEIGHBORS.map(|n| p + n));
        }
        for p in trunk.into_iter().chain(leaves) {
            self.set(p, AIR);
        }
    }

    /// Every changed voxel.
    pub fn diff(&self) -> Vec<(IVec3, VoxelId)> {
        let mut out = Vec::new();
        for i in 0..self.len() {
            let c = self.column_of(i);
            for y in 1..H {
                let (old, new) = (self.orig[i * H + y], self.vox[i * H + y]);
                if old != new {
                    out.push((IVec3::new(c.x, y as i32, c.y), new));
                }
            }
        }
        out
    }
}

/// What lies just under a surface block.
fn subsoil(top: VoxelId) -> VoxelId {
    match top {
        t if block::SAND.contains(&t) => block::SAND[1],
        t if block::STONE.contains(&t) => block::STONE[1],
        _ => block::DIRT[0],
    }
}

fn material_block(name: &str, p: IVec3, seed: u32, content: &Content) -> VoxelId {
    let t = value(p.as_vec3() / 5.0, seed);
    match name {
        "grass" => pick(block::GRASS, t),
        "dirt" => pick(block::DIRT, t),
        "stone" => pick(block::STONE, t),
        "sand" => pick(block::SAND, t),
        // One of the world's own blocks, or snow, as before there were any.
        name => content.id(name).unwrap_or(block::SNOW),
    }
}

fn smoothstep(edge: f32, d: f32) -> f32 {
    if edge <= 0.0 {
        return if d >= 0.0 { 1.0 } else { 0.0 };
    }
    let t = (d / edge).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// How strongly an op applies at an area-frame point: 1 inside, fading to 0 over
/// `edge` meters at the shape's border.
fn shape_weight(shape: Option<&OpArea>, whole: Vec2, edge: f32, p: Vec2) -> f32 {
    let (lo, size, ellipse) = match shape {
        Some(a) => (Vec2::new(a.x as f32, a.z as f32), Vec2::new(a.w as f32, a.d as f32), a.shape == "ellipse"),
        None => (Vec2::ZERO, whole, false),
    };
    let inside = if ellipse {
        let half = size / 2.0;
        let r = ((p - lo - half) / half).length();
        (1.0 - r) * half.min_element()
    } else {
        (p - lo).min(lo + size - p).min_element()
    };
    if inside <= 0.0 { 0.0 } else { smoothstep(edge, inside) }
}

/// Distance from `p` to a polyline, and how far along it the closest point is.
fn polyline_distance(points: &[Vec2], p: Vec2) -> (f32, f32) {
    let mut best = (f32::MAX, 0.0);
    let mut along = 0.0;
    for w in points.windows(2) {
        let (a, b) = (w[0], w[1]);
        let ab = b - a;
        let len = ab.length();
        let t = if len > 0.0 { ((p - a).dot(ab) / (len * len)).clamp(0.0, 1.0) } else { 0.0 };
        let d = p.distance(a + ab * t);
        if d < best.0 {
            best = (d, along + t * len);
        }
        along += len;
    }
    best
}

fn finite(v: Option<f64>) -> Option<f32> {
    v.filter(|v| v.is_finite()).map(|v| v as f32)
}

/// Checks a spec without touching any terrain. Errors are phrased for the agent.
pub fn validate(spec: &TerrainSpec, area: Area, limits: &TerrainLimits, content: &Content) -> Vec<String> {
    let TerrainLimits { max_ops, max_height_change_m, max_path_points, .. } = *limits;
    let mut errors = Vec::new();
    let whole = area.size_m();
    if spec.ops.is_empty() {
        errors.push("ops is empty".into());
    }
    if spec.ops.len() > max_ops {
        errors.push(format!("{} ops, at most {max_ops}", spec.ops.len()));
    }
    for (n, op) in spec.ops.iter().enumerate() {
        let at = format!("op {n} ({})", op.kind);
        if !OP_KINDS.contains(&op.kind.as_str()) && op.kind != CLEAR {
            errors.push(format!("{at}: unknown kind; use one of {}, {CLEAR}", OP_KINDS.join(", ")));
            continue;
        }
        for (name, v) in [
            ("amount_m", op.amount_m),
            ("level_m", op.level_m),
            ("depth_m", op.depth_m),
            ("width_m", op.width_m),
            ("edge_m", op.edge_m),
            ("density", op.density),
        ] {
            if v.is_some_and(|v| !v.is_finite()) {
                errors.push(format!("{at}: {name} must be a finite number"));
            }
        }
        if let Some(a) = &op.area {
            if a.shape != "rect" && a.shape != "ellipse" {
                errors.push(format!("{at}: area.shape must be \"rect\" or \"ellipse\""));
            }
            let fits = a.w > 0.0 && a.d > 0.0 && a.x + a.w > 0.0 && a.z + a.d > 0.0
                && a.x < whole.x as f64 && a.z < whole.y as f64;
            if !fits {
                errors.push(format!("{at}: area must overlap the selection (0..{:.1} × 0..{:.1} m)", whole.x, whole.y));
            }
        }
        let amount = finite(op.amount_m).unwrap_or(0.0).abs();
        if amount > max_height_change_m || finite(op.depth_m).unwrap_or(0.0).abs() > max_height_change_m {
            errors.push(format!("{at}: changes are at most {max_height_change_m} m"));
        }
        match op.kind.as_str() {
            "raise" | "lower" if op.amount_m.is_none() => errors.push(format!("{at}: needs amount_m")),
            "paint" | "path" if op.material.as_deref().is_some_and(|m| !MATERIALS.contains(&m) && content.id(m).is_none()) => {
                let own: Vec<&str> = content.own().map(|(_, b)| b.name.as_str()).collect();
                let own = if own.is_empty() { "; this world has no blocks of its own (the block tool makes one)".to_string() } else { format!(", or one of this world's own: {}", own.join(", ")) };
                errors.push(format!("{at}: material must be one of {}{own}", MATERIALS.join(", ")));
            }
            "paint" if op.material.is_none() => errors.push(format!("{at}: needs a material")),
            "scatter" | "clear" if op.item.as_deref().is_some_and(|m| !ITEMS.contains(&m)) => {
                errors.push(format!("{at}: item must be one of {}", ITEMS.join(", ")));
            }
            "scatter" if op.item.is_none() => errors.push(format!("{at}: needs an item")),
            "path" => {
                let points = op.points.as_deref().unwrap_or_default();
                if points.len() < 2 || points.len() > max_path_points || points.iter().any(|p| p.len() != 2 || p.iter().any(|v| !v.is_finite())) {
                    errors.push(format!("{at}: points needs 2..{max_path_points} [x, z] pairs"));
                }
            }
            _ => {}
        }
    }
    errors
}

/// Applies `spec` to the columns under `area` and returns the voxels that change.
pub fn compile(
    spec: &TerrainSpec,
    area: Area,
    terrain: Terrain,
    mut columns: Columns,
    limits: &TerrainLimits,
    content: &Content,
) -> Result<TerrainEdit, Vec<String>> {
    let errors = validate(spec, area, limits, content);
    if !errors.is_empty() {
        return Err(errors);
    }
    columns.cap = terrain.max_ground();
    let sea = terrain.sea_level();
    let voxel_of_level = |level: f32| voxel_of_level(terrain, level);
    let whole = area.size_m();
    let n = columns.len();
    let start = columns.ground.clone();
    let start_water = columns.water.clone();
    let mut stats = SculptStats::default();
    let local: Vec<Option<Vec2>> = (0..n)
        .map(|i| {
            let c = columns.column_of(i);
            (area.contains(c) && columns.orig[i * H] != AIR).then(|| area.to_local(c))
        })
        .collect();

    for (k, op) in spec.ops.iter().enumerate() {
        let seed = terrain.seed.wrapping_add(1000 + k as u32 * 17);
        let edge = finite(op.edge_m).unwrap_or(1.0).clamp(0.0, 20.0);
        let weights: Vec<f32> = local
            .iter()
            .map(|p| p.map_or(0.0, |p| shape_weight(op.area.as_ref(), whole, edge, p)))
            .collect();
        let covered = || (0..n).filter(|&i| weights[i] > 0.0);
        let mean_ground = || {
            let (sum, count) = covered().fold((0.0, 0.0), |(s, c), i| (s + columns.ground[i] as f32 * weights[i], c + weights[i]));
            if count > 0.0 { sum / count } else { sea as f32 }
        };
        let density = finite(op.density).unwrap_or(0.5).clamp(0.0, 1.0);

        match op.kind.as_str() {
            "raise" | "lower" => {
                let sign = if op.kind == "raise" { 1.0 } else { -1.0 };
                let voxels = finite(op.amount_m).unwrap_or(0.0).abs() / VOXEL_SIZE * sign;
                for i in covered().collect::<Vec<_>>() {
                    let g = columns.ground[i];
                    columns.set_ground(i, g + (voxels * weights[i]).round() as i32);
                }
            }
            "flatten" => {
                let target = finite(op.level_m).map_or_else(mean_ground, |l| voxel_of_level(l) as f32);
                for i in covered().collect::<Vec<_>>() {
                    let g = columns.ground[i] as f32;
                    columns.set_ground(i, (g + (target - g) * weights[i]).round() as i32);
                }
            }
            "terrace" => {
                let step = (finite(op.amount_m).unwrap_or(1.0).abs() / VOXEL_SIZE).round().max(2.0) as i32;
                for i in covered().collect::<Vec<_>>() {
                    let g = columns.ground[i];
                    let stepped = sea + (g - sea).div_euclid(step) * step;
                    columns.set_ground(i, (g as f32 + (stepped - g) as f32 * weights[i]).round() as i32);
                }
            }
            "smooth" => {
                let mut heights: Vec<f32> = columns.ground.iter().map(|&g| g as f32).collect();
                let size = columns.size;
                for _ in 0..1 + (density * 4.0).round() as i32 {
                    let prev = heights.clone();
                    for (i, height) in heights.iter_mut().enumerate() {
                        let c = IVec2::new(i as i32 % size.x, i as i32 / size.x);
                        let (mut sum, mut count) = (0.0_f32, 0.0_f32);
                        for dz in -3..=3 {
                            for dx in -3..=3 {
                                let q = c + IVec2::new(dx, dz);
                                if q.cmpge(IVec2::ZERO).all() && q.cmplt(size).all() {
                                    let j = (q.x + q.y * size.x) as usize;
                                    if columns.orig[j * H] != AIR {
                                        sum += prev[j];
                                        count += 1.0;
                                    }
                                }
                            }
                        }
                        *height = sum / count.max(1.0);
                    }
                }
                for i in covered().collect::<Vec<_>>() {
                    let g = columns.ground[i] as f32;
                    columns.set_ground(i, (g + (heights[i] - g) * weights[i]).round() as i32);
                }
            }
            "water" => {
                let level = finite(op.level_m).map_or_else(
                    || covered().filter(|&i| weights[i] > 0.5).map(|i| columns.ground[i]).min().unwrap_or(sea),
                    voxel_of_level,
                );
                let depth = finite(op.depth_m).unwrap_or(1.0).abs().max(VOXEL_SIZE) / VOXEL_SIZE;
                let mut trunks = Vec::new();
                for i in covered().collect::<Vec<_>>() {
                    let floor = level - (depth * weights[i]).round() as i32;
                    let g = columns.ground[i];
                    if g >= floor && weights[i] < 0.05 {
                        continue;
                    }
                    trunks.extend(columns.clear_plants(i));
                    columns.set_ground(i, g.min(floor));
                    let g = columns.ground[i];
                    let c = columns.column_of(i);
                    // Sandy banks and bed.
                    if g <= level {
                        for y in (g - 1).max(1)..=g {
                            columns.col(i)[y as usize] = pick(block::SAND, hash01(IVec3::new(c.x, y, c.y), seed));
                        }
                    }
                    for y in g + 1..=level {
                        columns.col(i)[y as usize] = block::WATER;
                    }
                    if g < level {
                        columns.water[i] = level;
                    }
                }
                columns.fell(trunks);
            }
            "paint" => {
                let material = op.material.as_deref().unwrap_or("grass");
                for i in covered().collect::<Vec<_>>() {
                    let c = columns.column_of(i);
                    let g = columns.ground[i];
                    // Patchy coverage: noise against density.
                    let patch = value(Vec3::new(c.x as f32, 0.0, c.y as f32) / 6.0, seed);
                    if patch * 0.999 > density * weights[i] + 0.001 && density < 1.0 {
                        continue;
                    }
                    let depth = if material == "sand" { 3 } else { 1 };
                    for y in (g - depth + 1).max(1)..=g {
                        columns.col(i)[y as usize] = material_block(material, IVec3::new(c.x, y, c.y), seed, content);
                    }
                    if material != "grass" {
                        columns.clear_plants(i);
                    }
                }
            }
            "scatter" => scatter(&mut columns, &weights, op.item.as_deref().unwrap_or("plants"), density, terrain, seed, &mut stats),
            "clear" => {
                let item = op.item.as_deref();
                let mut trunks = Vec::new();
                for i in covered().filter(|&i| weights[i] > 0.5).collect::<Vec<_>>() {
                    let c = columns.column_of(i);
                    let g = columns.ground[i];
                    if item != Some("tree") {
                        trunks.extend(columns.clear_plants(i));
                    } else {
                        trunks.extend((g + 1..H as i32).map(|y| IVec3::new(c.x, y, c.y)).filter(|p| block::BARK.contains(&columns.get(*p))));
                    }
                    if matches!(item, Some("rock")) {
                        // Rocks are stone sitting on the ground.
                        for y in g + 1..H as i32 {
                            let p = IVec3::new(c.x, y, c.y);
                            if block::STONE.contains(&columns.get(p)) {
                                columns.set(p, AIR);
                            }
                        }
                    }
                }
                if matches!(item, None | Some("tree") | Some("plants")) {
                    columns.fell(trunks);
                }
            }
            "path" => {
                let points: Vec<Vec2> = op.points.iter().flatten().map(|p| Vec2::new(p[0] as f32, p[1] as f32)).collect();
                let half = finite(op.width_m).unwrap_or(1.5).clamp(VOXEL_SIZE, 6.0) / 2.0;
                let material = op.material.as_deref().unwrap_or("dirt");
                // The path's own height: ground sampled along it, then eased.
                let length: f32 = points.windows(2).map(|w| w[0].distance(w[1])).sum();
                let samples = (length / VOXEL_SIZE).ceil().max(1.0) as usize + 1;
                let mut profile: Vec<f32> = (0..samples)
                    .map(|s| {
                        let at = point_along(&points, s as f32 * VOXEL_SIZE);
                        columns.index(area.to_world(at)).map_or(sea as f32, |i| columns.ground[i] as f32)
                    })
                    .collect();
                for _ in 0..3 {
                    let prev = profile.clone();
                    for (s, h) in profile.iter_mut().enumerate() {
                        let lo = s.saturating_sub(6);
                        let hi = (s + 6).min(prev.len() - 1);
                        *h = prev[lo..=hi].iter().sum::<f32>() / (hi - lo + 1) as f32;
                    }
                }
                let mut trunks = Vec::new();
                for (i, p) in local.iter().enumerate() {
                    let Some(p) = *p else { continue };
                    let (distance, along) = polyline_distance(&points, p);
                    let blend = 1.0 - smoothstep(edge.max(0.5), distance - half);
                    if blend <= 0.0 {
                        continue;
                    }
                    let h = profile[((along / VOXEL_SIZE).round() as usize).min(samples - 1)];
                    let g = columns.ground[i] as f32;
                    columns.set_ground(i, (g + (h - g) * blend).round() as i32);
                    if distance <= half + 0.25 {
                        trunks.extend(columns.clear_plants(i));
                    }
                    if distance <= half {
                        let c = columns.column_of(i);
                        let g = columns.ground[i];
                        columns.col(i)[g as usize] = material_block(material, IVec3::new(c.x, g, c.y), seed, content);
                    }
                }
                columns.fell(trunks);
            }
            _ => unreachable!("validated"),
        }
    }

    for i in 0..n {
        let delta = (columns.ground[i] - start[i]) as f32 * VOXEL_SIZE;
        stats.raised = stats.raised.max(delta);
        stats.lowered = stats.lowered.max(-delta);
        // New water only, not sea or lakes that were already there.
        let wet = columns.water[i] > columns.ground[i];
        let was_wet = start_water[i] > start[i];
        if wet && local[i].is_some() && (!was_wet || columns.water[i] != start_water[i]) {
            stats.water_columns += 1;
        }
    }
    let edits = columns.diff();
    stats.changed = edits.len();
    let mut errors = Vec::new();
    if stats.changed > limits.max_changed_voxels {
        errors.push(format!("changes {} voxels, at most {}", stats.changed, limits.max_changed_voxels));
    }
    if stats.trees > limits.max_trees {
        errors.push(format!("adds {} trees, at most {}", stats.trees, limits.max_trees));
    }
    if errors.is_empty() { Ok(TerrainEdit { edits, stats }) } else { Err(errors) }
}

fn point_along(points: &[Vec2], mut distance: f32) -> Vec2 {
    for w in points.windows(2) {
        let len = w[0].distance(w[1]);
        if distance <= len && len > 0.0 {
            return w[0].lerp(w[1], distance / len);
        }
        distance -= len;
    }
    *points.last().unwrap_or(&Vec2::ZERO)
}

fn scatter(columns: &mut Columns, weights: &[f32], item: &str, density: f32, terrain: Terrain, seed: u32, stats: &mut SculptStats) {
    let dry = |columns: &Columns, i: usize| columns.water[i] <= columns.ground[i];
    let soft = |id: VoxelId| block::GRASS.contains(&id) || block::DIRT.contains(&id) || id == block::SNOW;

    // Single-voxel plants: a chance per column.
    if matches!(item, "flowers" | "grass" | "plants") {
        for (i, &weight) in weights.iter().enumerate() {
            if weight <= 0.0 || !dry(columns, i) {
                continue;
            }
            let c = columns.column_of(i);
            let g = columns.ground[i];
            let above = IVec3::new(c.x, g + 1, c.y);
            if !block::GRASS.contains(&columns.get(above - IVec3::Y)) || columns.get(above) != AIR {
                continue;
            }
            let roll = hash01(above, seed);
            let chance = density * weight;
            let flower = || pick(block::FLOWERS, value(above.as_vec3() / 8.0, seed + 1));
            let id = match item {
                "flowers" if roll < chance * 0.35 => flower(),
                "grass" if roll < chance * 0.6 => block::TUFT,
                "plants" if roll < chance * 0.08 => flower(),
                "plants" if roll < chance * 0.4 => block::TUFT,
                _ => continue,
            };
            columns.set(above, id);
        }
        return;
    }

    // Objects: at most one per jittered cell, so they don't pile up.
    let cell = match item {
        "tree" => 16,
        "bush" => 9,
        _ => 11,
    };
    let lo = columns.min.div_euclid(IVec2::splat(cell));
    let hi = (columns.min + columns.size).div_euclid(IVec2::splat(cell));
    for cz in lo.y..=hi.y {
        for cx in lo.x..=hi.x {
            let key = IVec3::new(cx, item.len() as i32, cz);
            let jitter = |k: u32| (hash01(key, seed + k) * (cell - 4) as f32) as i32 + 2;
            let c = IVec2::new(cx * cell + jitter(1), cz * cell + jitter(2));
            let Some(i) = columns.index(c) else { continue };
            // Density 1 fills every cell; small values still give a few.
            let chance = (density * 1.6).min(1.0) * weights[i];
            if hash01(key, seed + 3) >= chance || !dry(columns, i) {
                continue;
            }
            let g = columns.ground[i];
            let top = columns.get(IVec3::new(c.x, g, c.y));
            let base = IVec3::new(c.x, g, c.y);
            match item {
                "tree" if soft(top) => {
                    let mut grid = VoxelGrid::new(UVec3::new(25, 40, 25));
                    let min = base + IVec3::new(-12, 1, -12);
                    terrain.grow_tree(&mut grid, min, base, key + IVec3::new(0, seed as i32 & 0xff, 0));
                    copy_into(columns, &grid, min);
                    stats.trees += 1;
                }
                "bush" if soft(top) => {
                    let r = 1.6 + hash01(key, seed + 4) * 1.2;
                    blob(columns, base.as_vec3() + Vec3::new(0.5, 1.0 + r * 0.6, 0.5), Vec3::new(r, r * 0.8, r), seed, |p, t| {
                        pick(block::LEAVES, (t + hash01(p, seed) * 0.4).min(0.99))
                    });
                }
                "rock" => {
                    let r = 1.2 + hash01(key, seed + 5) * 2.2;
                    blob(columns, base.as_vec3() + Vec3::new(0.5, 0.6 + r * 0.25, 0.5), Vec3::new(r * 1.2, r * 0.8, r), seed, |p, _| {
                        pick(block::STONE, value(p.as_vec3() / 2.0, seed + 6))
                    });
                }
                _ => {}
            }
        }
    }
}

/// A noisy ellipsoid of voxels, only into open space.
fn blob(columns: &mut Columns, center: Vec3, radii: Vec3, seed: u32, color: impl Fn(IVec3, f32) -> VoxelId) {
    let lo = (center - radii - 1.0).floor().as_ivec3();
    let hi = (center + radii + 1.0).ceil().as_ivec3();
    for z in lo.z..=hi.z {
        for y in lo.y..=hi.y {
            for x in lo.x..=hi.x {
                let p = IVec3::new(x, y, z);
                let q = (p.as_vec3() + 0.5 - center) / radii;
                let wobble = (value(p.as_vec3() / 1.7, seed + 7) - 0.5) * 0.5;
                if q.length() > 1.0 + wobble {
                    continue;
                }
                if matches!(columns.get(p), AIR | block::TUFT) || block::FLOWERS.contains(&columns.get(p)) {
                    columns.set(p, color(p, 1.0 - (q.y + 1.0) / 2.0));
                }
            }
        }
    }
}

fn copy_into(columns: &mut Columns, grid: &VoxelGrid, min: IVec3) {
    let size = grid.size().as_ivec3();
    for z in 0..size.z {
        for y in 0..size.y {
            for x in 0..size.x {
                let l = IVec3::new(x, y, z);
                let id = grid.get(l);
                if id == AIR {
                    continue;
                }
                let p = min + l;
                let here = columns.get(p);
                if here == AIR || here == block::TUFT || block::FLOWERS.contains(&here) {
                    columns.set(p, id);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TERRAIN: Terrain = Terrain { seed: 1337, flat: false, version: crate::terrain::TERRAIN_VERSION, recipe: crate::recipe::Recipe::NATURAL };

    fn columns(area: Area) -> Columns {
        Columns::read(area, |x, z, out| {
            let grid = TERRAIN.generate(IVec3::new(x, 0, z), UVec3::new(1, WORLD_HEIGHT as u32, 1));
            for (y, v) in out.iter_mut().enumerate() {
                *v = grid.get(IVec3::new(0, y as i32, 0));
            }
            true
        })
        .unwrap()
    }

    fn op(kind: &str) -> TerrainOp {
        TerrainOp {
            kind: kind.into(),
            area: None,
            points: None,
            amount_m: None,
            level_m: None,
            depth_m: None,
            width_m: None,
            edge_m: Some(0.0),
            material: None,
            item: None,
            density: None,
        }
    }

    fn run(area: Area, ops: Vec<TerrainOp>) -> (Columns, TerrainEdit) {
        let spec = TerrainSpec { name: "test".into(), ops, about: None };
        let edit = compile(&spec, area, TERRAIN, columns(area), &TerrainLimits::default(), &Content::default()).unwrap();
        let mut after = columns(area);
        for (p, id) in &edit.edits {
            after.set(*p, *id);
        }
        for i in 0..after.len() {
            after.rescan(i);
        }
        (after, edit)
    }

    /// An inland spot above sea level.
    fn dry_area() -> Area {
        let mut x = 0;
        while TERRAIN.height(x, 0) <= TERRAIN.sea_level() + 4 {
            x += 7;
        }
        Area { min: [x, 0], size: [24, 24] }
    }

    #[test]
    fn flatten_levels_the_area() {
        let area = dry_area();
        let mut flat = op("flatten");
        flat.level_m = Some(level_of(TERRAIN, TERRAIN.height(area.min[0], 0)) as f64);
        let (after, edit) = run(area, vec![flat]);
        assert!(edit.stats.changed > 0);
        let target = TERRAIN.height(area.min[0], 0);
        for z in 0..24 {
            for x in 0..24 {
                let (g, _, _) = after.ground_at(area.min() + IVec2::new(x, z)).unwrap();
                assert_eq!(g, target);
            }
        }
        // Outside the area nothing moved.
        let outside = area.min() - IVec2::splat(3);
        assert_eq!(after.ground_at(outside).unwrap().0, TERRAIN.height(outside.x, outside.y));
    }

    #[test]
    fn water_fills_a_basin() {
        let area = dry_area();
        let mut pond = op("water");
        pond.area = Some(OpArea { shape: "ellipse".into(), x: 1.0, z: 1.0, w: 4.0, d: 4.0 });
        pond.depth_m = Some(1.0);
        pond.edge_m = Some(1.0);
        let (after, edit) = run(area, vec![pond]);
        assert!(edit.stats.water_columns > 20, "{:?}", edit.stats);
        let center = area.to_world(Vec2::splat(3.0));
        let (g, w, _) = after.ground_at(center).unwrap();
        assert!(w >= g + 3, "ground {g} water {w}");
    }

    #[test]
    fn raise_is_limited_and_validated() {
        let area = dry_area();
        let mut up = op("raise");
        up.amount_m = Some(20.0);
        let spec = TerrainSpec { name: "too much".into(), ops: vec![up, op("teleport")], about: None };
        let errors = validate(&spec, area, &TerrainLimits::default(), &Content::default());
        assert_eq!(errors.len(), 2, "{errors:?}");

        let mut up = op("raise");
        up.amount_m = Some(2.0);
        let (after, edit) = run(area, vec![up]);
        assert_eq!(edit.stats.raised, 2.0);
        let c = area.min() + IVec2::splat(5);
        assert_eq!(after.ground_at(c).unwrap().0, TERRAIN.height(c.x, c.y) + 8);
    }

    #[test]
    fn land_is_painted_with_a_worlds_own_block() {
        let area = dry_area();
        let mut paint = op("paint");
        paint.material = Some("lava".into());
        let spec = TerrainSpec { name: "lava field".into(), ops: vec![paint], about: None };
        let none = Content::default();
        assert!(validate(&spec, area, &TerrainLimits::default(), &none)[0].contains("no blocks of its own"));

        let mut content = Content::default();
        let lava = |name: &str| crate::content::BlockDef { name: name.into(), hex: "#ff5a00".into(), material: "glow".into(), tune: Default::default(), variation: None, gone: false };
        content.define(lava("ice")).unwrap();
        content.define(lava("lava")).unwrap();
        let edit = compile(&spec, area, TERRAIN, columns(area), &TerrainLimits::default(), &content).expect("lava is a block here");
        let id = content.id("lava").unwrap();
        assert!(!edit.edits.is_empty() && edit.edits.iter().all(|(_, block)| *block == id || *block == AIR), "only lava, and plants cleared");
        // It's ground from then on: the next change stands on it.
        let mut after = columns(area);
        for (p, id) in &edit.edits {
            after.set(*p, *id);
        }
        for i in 0..after.len() {
            after.rescan(i);
        }
        let c = area.min() + IVec2::splat(5);
        assert_eq!(after.ground_at(c).unwrap().0, TERRAIN.height(c.x, c.y));
        assert_eq!(after.get(IVec3::new(c.x, TERRAIN.height(c.x, c.y), c.y)), id);
    }

    #[test]
    fn reshaping_dry_land_never_makes_water() {
        let area = dry_area();
        let mut up = op("raise");
        up.amount_m = Some(2.0);
        up.edge_m = Some(3.0);
        let mut down = op("lower");
        down.amount_m = Some(1.0);
        down.area = Some(OpArea { shape: "ellipse".into(), x: 2.0, z: 2.0, w: 3.0, d: 3.0 });
        let mut smooth = op("smooth");
        smooth.density = Some(1.0);
        let (_, edit) = run(area, vec![up, down, smooth]);
        assert_eq!(edit.stats.water_columns, 0);
        assert!(edit.edits.iter().all(|(_, id)| *id != block::WATER));
    }

    #[test]
    fn path_paints_its_material() {
        let area = dry_area();
        let mut path = op("path");
        path.points = Some(vec![vec![0.5, 3.0], vec![5.5, 3.0]]);
        path.material = Some("stone".into());
        path.width_m = Some(1.0);
        let (after, _) = run(area, vec![path]);
        let (_, _, top) = after.ground_at(area.to_world(Vec2::new(3.0, 3.0))).unwrap();
        assert!(block::STONE.contains(&top), "top {top}");
    }

    #[test]
    fn scatter_and_clear_trees() {
        let area = dry_area();
        let mut trees = op("scatter");
        trees.item = Some("tree".into());
        trees.density = Some(1.0);
        let (_, edit) = run(area, vec![trees.clone()]);
        assert!(edit.stats.trees > 0);
        let mut clear = op("clear");
        clear.item = Some("tree".into());
        let (after, _) = run(area, vec![trees, clear]);
        let bark = (0..after.len()).flat_map(|i| after.vox[i * H..(i + 1) * H].to_vec()).filter(|id| block::BARK.contains(id)).count();
        let before = columns(area);
        let bark_before = (0..before.len()).flat_map(|i| before.vox[i * H..(i + 1) * H].to_vec()).filter(|id| block::BARK.contains(id)).count();
        assert!(bark <= bark_before, "bark {bark} vs {bark_before}");
    }
}
