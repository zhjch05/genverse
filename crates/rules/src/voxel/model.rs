//! Data-driven voxel models: a definition (a `.vox.ron` file, or a compiled build spec)
//! declares surfaces, a palette, and an ordered list of operations (shapes and
//! generators) that are rasterized into a grid.

use std::{collections::BTreeMap, sync::Arc};

use bevy_color::prelude::*;
use bevy_math::prelude::*;
use serde::Deserialize;

use super::{
    grid::{AIR, VoxelGrid, VoxelId},
    noise::{fbm, fbm_xz, hash01},
    palette::{Palette, PaletteEntry, Surface},
};

// ---------------------------------------------------------------------------
// File format
// ---------------------------------------------------------------------------

pub type V3 = (f32, f32, f32);

#[derive(Deserialize)]
pub struct ModelDef {
    pub voxel_size: f32,
    pub size: (u32, u32, u32),
    /// Point (in voxel coordinates) placed at the entity's origin.
    pub pivot: V3,
    pub surfaces: Vec<SurfaceDef>,
    pub palette: Vec<PaletteDef>,
    pub ops: Vec<OpDef>,
    #[serde(default)]
    pub lights: Vec<LightDef>,
    #[serde(default)]
    pub parts: Vec<PartDef>,
}

/// A named box of voxels cut out as its own model, rotating around `joint`.
#[derive(Deserialize)]
pub struct PartDef {
    pub name: String,
    pub min: (i32, i32, i32),
    pub max: (i32, i32, i32),
    pub joint: V3,
    /// Part this one is attached to; its joint moves with the parent.
    #[serde(default)]
    pub parent: Option<String>,
}

fn one() -> f32 {
    1.0
}

fn default_roughness() -> f32 {
    0.85
}

fn default_reflectance() -> f32 {
    0.4
}

#[derive(Deserialize)]
pub struct SurfaceDef {
    pub name: String,
    #[serde(default = "default_roughness")]
    pub roughness: f32,
    #[serde(default)]
    pub metallic: f32,
    #[serde(default = "default_reflectance")]
    pub reflectance: f32,
    /// `(hex color, strength)`.
    #[serde(default)]
    pub emissive: Option<(String, f32)>,
    #[serde(default = "one")]
    pub alpha: f32,
}

#[derive(Deserialize)]
pub struct PaletteDef {
    pub name: String,
    pub color: String,
    pub surface: String,
    #[serde(default)]
    pub variation: f32,
}

/// What to write into each covered cell.
#[derive(Deserialize, Clone)]
pub enum Fill {
    Solid(String),
    /// Picks among `mats` using 3D fractal noise with features about `scale` voxels wide.
    Noise { mats: Vec<String>, scale: f32, #[serde(default)] seed: u32 },
}

#[derive(Deserialize, Clone, Copy)]
pub struct NoiseDef {
    pub scale: f32,
    pub amplitude: f32,
    #[serde(default)]
    pub seed: u32,
}

#[derive(Deserialize)]
pub enum Geom {
    Box { min: V3, max: V3 },
    Ellipsoid { center: V3, radii: V3 },
    /// Tapered capsule between two points: trunks, branches, pillars.
    Capsule { a: V3, b: V3, radius_a: f32, radius_b: f32 },
    /// Vertical cylinder standing on `base`.
    Cylinder { base: V3, radius: f32, height: f32 },
}

#[derive(Deserialize)]
pub enum ScatterMode {
    /// Stack new voxels on top of the matched surface voxel.
    Above,
    /// Recolor the matched surface voxel itself.
    Replace,
}

#[derive(Deserialize)]
pub enum OpDef {
    Shape {
        geom: Geom,
        fill: Fill,
        /// Distorts the shape boundary for organic, non-blocky silhouettes.
        #[serde(default)]
        noise: Option<NoiseDef>,
        /// Only overwrite cells currently holding one of these materials.
        #[serde(default)]
        mask: Option<Vec<String>>,
    },
    /// A floating island: noisy heightmap on top, tapering rocky underside below.
    Island {
        center: (f32, f32),
        top_y: f32,
        radius: f32,
        depth: f32,
        hills: f32,
        /// Top-down layers as `(fill, thickness)`; anything deeper uses `core`.
        layers: Vec<(Fill, u32)>,
        core: Fill,
        #[serde(default)]
        seed: u32,
    },
    /// Decorates exposed top faces of `on` materials: flowers, grass tufts, petals.
    Scatter {
        on: Vec<String>,
        fill: Fill,
        chance: f32,
        mode: ScatterMode,
        #[serde(default = "default_height")]
        height: (u32, u32),
        #[serde(default)]
        region: Option<(V3, V3)>,
        #[serde(default)]
        seed: u32,
    },
    /// Painted voxels, written cell by cell like pixel art. Each layer is a picture of
    /// one z slice, as seen from the front: `layers[i]` is z = `at.z + i`, its first row
    /// is the top (y = `at.y + rows - 1`) and its last row sits at `at.y`, and each row
    /// runs along +x from `at.x`. Every character is a `legend` key naming a material
    /// (`"air"` carves); `.` and space leave the cell as it was. Rows may differ in length.
    Voxels {
        at: (i32, i32, i32),
        legend: BTreeMap<char, String>,
        layers: Vec<Vec<String>>,
    },
    /// Removes `mats` voxels with nothing solid underneath, bottom-up, so liquids rest on ground.
    Settle { mats: Vec<String>, region: (V3, V3) },
    /// Pours `mat` over ledges: every `mat` voxel next to a drop starts a falling column
    /// up to `length` voxels long, `thickness` voxels away from the ledge.
    Spill {
        mat: String,
        region: (V3, V3),
        length: u32,
        #[serde(default = "default_thickness")]
        thickness: u32,
    },
}

fn default_thickness() -> u32 {
    1
}

fn default_height() -> (u32, u32) {
    (1, 1)
}

#[derive(Deserialize)]
pub struct LightDef {
    pub position: V3,
    pub color: String,
    /// Luminous power in lumens.
    pub intensity: f32,
    pub range: f32,
}

// ---------------------------------------------------------------------------
// Loaded asset
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ModelLight {
    /// Voxel coordinates.
    pub position: Vec3,
    pub color: Color,
    pub intensity: f32,
    pub range: f32,
}

/// A sub-model for animation: voxels in `min..max`, rotating around `joint`.
#[derive(Clone, Debug)]
pub struct ModelPart {
    pub name: String,
    pub min: IVec3,
    pub max: IVec3,
    /// Voxel coordinates in the full model.
    pub joint: Vec3,
    pub parent: Option<String>,
}

/// A rasterized model.
#[derive(Clone)]
pub struct ModelData {
    pub grid: VoxelGrid,
    pub palette: Arc<Palette>,
    pub voxel_size: f32,
    pub pivot: Vec3,
    pub lights: Vec<ModelLight>,
    pub parts: Vec<ModelPart>,
}

// ---------------------------------------------------------------------------
// Rasterization
// ---------------------------------------------------------------------------

fn v3(t: V3) -> Vec3 {
    Vec3::new(t.0, t.1, t.2)
}

fn color(hex: &str) -> Result<Srgba, String> {
    Srgba::hex(hex).map_err(|e| format!("bad color {hex:?}: {e}"))
}

pub fn build(def: ModelDef) -> Result<ModelData, String> {
    build_ordered(def, None)
}

/// [`build`], also noting in `order` which op last changed each cell (in the grid's
/// layout; cells no op changed keep 0).
pub fn build_ordered(def: ModelDef, mut order: Option<&mut Vec<u16>>) -> Result<ModelData, String> {
    let surfaces = def
        .surfaces
        .iter()
        .map(|s| {
            let emissive = match &s.emissive {
                Some((hex, strength)) => LinearRgba::from(color(hex)?) * *strength,
                None => LinearRgba::BLACK,
            };
            Ok(Surface {
                name: s.name.clone(),
                roughness: s.roughness,
                metallic: s.metallic,
                reflectance: s.reflectance,
                emissive,
                alpha: s.alpha,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;

    let mut entries = vec![PaletteEntry {
        name: "air".into(),
        color: LinearRgba::NONE,
        surface: 0,
        variation: 0.0,
    }];
    for p in &def.palette {
        let surface = surfaces
            .iter()
            .position(|s| s.name == p.surface)
            .ok_or_else(|| format!("palette entry {:?} uses unknown surface {:?}", p.name, p.surface))?;
        entries.push(PaletteEntry {
            name: p.name.clone(),
            color: color(&p.color)?.into(),
            surface,
            variation: p.variation,
        });
    }
    if entries.len() > 256 {
        return Err("a palette holds at most 255 entries".into());
    }
    let palette = Palette { surfaces, entries };

    let mut grid = VoxelGrid::new(UVec3::new(def.size.0, def.size.1, def.size.2));
    if let Some(order) = order.as_deref_mut() {
        *order = vec![0; grid.cells().len()];
    }
    for (i, op) in def.ops.iter().enumerate() {
        let before = order.is_some().then(|| grid.cells().to_vec());
        apply_op(&mut grid, &palette, op)?;
        if let (Some(order), Some(before)) = (order.as_deref_mut(), before) {
            for (cell, _) in before.iter().zip(grid.cells()).enumerate().filter(|(_, (was, is))| was != is) {
                order[cell] = i as u16;
            }
        }
    }

    let lights = def
        .lights
        .iter()
        .map(|l| {
            Ok(ModelLight {
                position: v3(l.position),
                color: color(&l.color)?.into(),
                intensity: l.intensity,
                range: l.range,
            })
        })
        .collect::<Result<_, String>>()?;

    let parts = def
        .parts
        .iter()
        .map(|p| ModelPart {
            name: p.name.clone(),
            min: IVec3::new(p.min.0, p.min.1, p.min.2),
            max: IVec3::new(p.max.0, p.max.1, p.max.2),
            joint: v3(p.joint),
            parent: p.parent.clone(),
        })
        .collect();

    Ok(ModelData {
        grid,
        palette: Arc::new(palette),
        voxel_size: def.voxel_size,
        pivot: v3(def.pivot),
        lights,
        parts,
    })
}

/// A [`Fill`] with material names resolved to palette ids.
enum ResolvedFill {
    Solid(VoxelId),
    Noise { ids: Vec<VoxelId>, scale: f32, seed: u32 },
}

impl ResolvedFill {
    fn new(fill: &Fill, palette: &Palette) -> Result<Self, String> {
        Ok(match fill {
            Fill::Solid(name) => Self::Solid(lookup(palette, name)?),
            Fill::Noise { mats, scale, seed } => Self::Noise {
                ids: mats.iter().map(|m| lookup(palette, m)).collect::<Result<_, _>>()?,
                scale: *scale,
                seed: *seed,
            },
        })
    }

    fn sample(&self, p: IVec3) -> VoxelId {
        match self {
            Self::Solid(id) => *id,
            Self::Noise { ids, scale, seed } => {
                // Stretch the noise contrast so every material gets a fair share.
                let n = ((fbm(p.as_vec3() / *scale, *seed) - 0.5) * 2.2 + 0.5).clamp(0.0, 0.999);
                ids[(n * ids.len() as f32) as usize]
            }
        }
    }
}

fn lookup(palette: &Palette, name: &str) -> Result<VoxelId, String> {
    palette.find(name).ok_or_else(|| format!("unknown material {name:?}"))
}

fn apply_op(grid: &mut VoxelGrid, palette: &Palette, op: &OpDef) -> Result<(), String> {
    match op {
        OpDef::Shape { geom, fill, noise, mask } => {
            let fill = ResolvedFill::new(fill, palette)?;
            let mask = mask
                .as_ref()
                .map(|m| m.iter().map(|n| lookup(palette, n)).collect::<Result<Vec<_>, _>>())
                .transpose()?;
            let (min, max) = geom_bounds(geom, noise.as_ref());
            for_each_cell(grid, min, max, |grid, p| {
                if let Some(mask) = &mask
                    && !mask.contains(&grid.get(p))
                {
                    return;
                }
                let center = p.as_vec3() + 0.5;
                let displacement = noise.map_or(0.0, |n| {
                    (fbm(center / n.scale, n.seed) - 0.5) * 2.0 * n.amplitude
                });
                if geom_distance(geom, center) <= 1.0 + displacement {
                    grid.set(p, fill.sample(p));
                }
            });
        }
        OpDef::Island { center, top_y, radius, depth, hills, layers, core, seed } => {
            let layers = layers
                .iter()
                .map(|(f, t)| Ok((ResolvedFill::new(f, palette)?, *t as i32)))
                .collect::<Result<Vec<_>, String>>()?;
            let core = ResolvedFill::new(core, palette)?;
            let c = Vec2::new(center.0, center.1);
            let reach = (radius * 1.25).ceil() as i32;
            for z in (c.y as i32 - reach)..=(c.y as i32 + reach) {
                for x in (c.x as i32 - reach)..=(c.x as i32 + reach) {
                    let xz = Vec2::new(x as f32 + 0.5, z as f32 + 0.5);
                    let n2 = |s: f32, k: u32| fbm_xz(xz / s, seed + k);
                    let edge = xz.distance(c) / radius + (n2(18.0, 0) - 0.5) * 0.35;
                    if edge >= 1.0 {
                        continue;
                    }
                    let t = 1.0 - edge;
                    let rim = 1.0 - smoothstep(0.0, 0.18, t);
                    let top = top_y + (n2(28.0, 1) - 0.5) * 2.0 * hills * smoothstep(0.0, 0.4, t)
                        - rim * 4.0;
                    let drip = 0.45 + 0.55 * n2(7.0, 2);
                    let bottom = top_y - depth * t.powf(0.7) * drip;
                    let (top, bottom) = (top.round() as i32, bottom.round() as i32);
                    for y in bottom..=top {
                        let p = IVec3::new(x, y, z);
                        let mut from_top = top - y;
                        let mut id = None;
                        for (fill, thickness) in &layers {
                            if from_top < *thickness {
                                id = Some(fill.sample(p));
                                break;
                            }
                            from_top -= thickness;
                        }
                        grid.set(p, id.unwrap_or_else(|| core.sample(p)));
                    }
                }
            }
        }
        OpDef::Scatter { on, fill, chance, mode, height, region, seed } => {
            let on = on.iter().map(|n| lookup(palette, n)).collect::<Result<Vec<_>, _>>()?;
            let fill = ResolvedFill::new(fill, palette)?;
            let size = grid.size().as_ivec3();
            let (min, max) = match region {
                Some((a, b)) => (v3(*a).floor().as_ivec3(), v3(*b).ceil().as_ivec3()),
                None => (IVec3::ZERO, size),
            };
            for z in min.z.max(0)..max.z.min(size.z) {
                for x in min.x.max(0)..max.x.min(size.x) {
                    for y in (min.y.max(0)..max.y.min(size.y)).rev() {
                        let p = IVec3::new(x, y, z);
                        let id = grid.get(p);
                        if !on.contains(&id) || grid.get(p + IVec3::Y) != AIR {
                            continue;
                        }
                        if hash01(p, *seed) >= *chance {
                            continue;
                        }
                        match mode {
                            ScatterMode::Replace => grid.set(p, fill.sample(p)),
                            ScatterMode::Above => {
                                let span = height.1.saturating_sub(height.0) + 1;
                                let h = height.0 + (hash01(p, seed + 1) * span as f32) as u32;
                                for dy in 1..=h as i32 {
                                    let q = p + IVec3::Y * dy;
                                    if grid.get(q) == AIR {
                                        grid.set(q, fill.sample(q));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        OpDef::Voxels { at, legend, layers } => {
            let mut ids = BTreeMap::new();
            for (&key, name) in legend {
                if key == '.' || key == ' ' {
                    return Err(format!("legend key {key:?} is reserved for \"leave as is\""));
                }
                ids.insert(key, lookup(palette, name)?);
            }
            let at = IVec3::new(at.0, at.1, at.2);
            for (dz, rows) in layers.iter().enumerate() {
                for (row, line) in rows.iter().enumerate() {
                    let y = (rows.len() - 1 - row) as i32;
                    for (dx, key) in line.chars().enumerate() {
                        if key == '.' || key == ' ' {
                            continue;
                        }
                        let id = *ids.get(&key).ok_or_else(|| {
                            format!("painted voxel {key:?} (layer {dz}, row {row}) is not in the legend")
                        })?;
                        grid.set(at + IVec3::new(dx as i32, y, dz as i32), id);
                    }
                }
            }
        }
        OpDef::Settle { mats, region } => {
            let mats = mats.iter().map(|n| lookup(palette, n)).collect::<Result<Vec<_>, _>>()?;
            let (min, max) = (v3(region.0), v3(region.1));
            // Cells are visited y-ascending, so removals cascade upward within one pass.
            for_each_cell(grid, min, max, |grid, p| {
                if mats.contains(&grid.get(p)) && grid.get(p - IVec3::Y) == AIR {
                    grid.set(p, AIR);
                }
            });
        }
        OpDef::Spill { mat, region, length, thickness } => {
            let id = lookup(palette, mat)?;
            let mut sources = Vec::new();
            for_each_cell(grid, v3(region.0), v3(region.1), |grid, p| {
                if grid.get(p) == id {
                    sources.push(p);
                }
            });
            for p in sources {
                for dir in [IVec3::X, IVec3::NEG_X, IVec3::Z, IVec3::NEG_Z] {
                    let lip = p + dir;
                    if grid.get(lip) != AIR || grid.get(lip - IVec3::Y) != AIR {
                        continue;
                    }
                    for t in 0..*thickness as i32 {
                        for dy in 0..*length as i32 {
                            let q = lip + dir * t - IVec3::Y * dy;
                            if grid.get(q) != AIR && grid.get(q) != id {
                                break;
                            }
                            grid.set(q, id);
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn for_each_cell(grid: &mut VoxelGrid, min: Vec3, max: Vec3, mut f: impl FnMut(&mut VoxelGrid, IVec3)) {
    let size = grid.size().as_ivec3();
    let lo = min.floor().as_ivec3().max(IVec3::ZERO);
    let hi = max.ceil().as_ivec3().min(size);
    for z in lo.z..hi.z {
        for y in lo.y..hi.y {
            for x in lo.x..hi.x {
                f(grid, IVec3::new(x, y, z));
            }
        }
    }
}

/// Conservative bounds, padded for noise displacement.
fn geom_bounds(geom: &Geom, noise: Option<&NoiseDef>) -> (Vec3, Vec3) {
    let grow = 1.0 + noise.map_or(0.0, |n| n.amplitude);
    match geom {
        Geom::Box { min, max } => (v3(*min), v3(*max)),
        Geom::Ellipsoid { center, radii } => {
            let r = v3(*radii) * grow;
            (v3(*center) - r, v3(*center) + r)
        }
        Geom::Capsule { a, b, radius_a, radius_b } => {
            let r = Vec3::splat(radius_a.max(*radius_b) * grow);
            (v3(*a).min(v3(*b)) - r, v3(*a).max(v3(*b)) + r)
        }
        Geom::Cylinder { base, radius, height } => {
            let r = radius * grow;
            let b = v3(*base);
            (b - Vec3::new(r, 0.0, r), b + Vec3::new(r, *height, r))
        }
    }
}

/// Normalized distance: `<= 1.0` is inside the shape.
fn geom_distance(geom: &Geom, p: Vec3) -> f32 {
    match geom {
        Geom::Box { min, max } => {
            let inside = p.cmpge(v3(*min)).all() && p.cmplt(v3(*max)).all();
            if inside { 0.0 } else { f32::INFINITY }
        }
        Geom::Ellipsoid { center, radii } => ((p - v3(*center)) / v3(*radii)).length(),
        Geom::Capsule { a, b, radius_a, radius_b } => {
            let (a, b) = (v3(*a), v3(*b));
            let ab = b - a;
            let t = ((p - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0);
            let radius = radius_a + (radius_b - radius_a) * t;
            p.distance(a + ab * t) / radius
        }
        Geom::Cylinder { base, radius, height } => {
            let b = v3(*base);
            if p.y < b.y || p.y > b.y + height {
                f32::INFINITY
            } else {
                Vec2::new(p.x - b.x, p.z - b.z).length() / radius
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> ModelDef {
        ron::Options::default()
            .with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
            .from_str(text)
            .unwrap()
    }

    fn model(ops: &str) -> Result<ModelData, String> {
        build(parse(&format!(
            r##"(
                voxel_size: 0.1,
                size: (4, 3, 2),
                pivot: (0, 0, 0),
                surfaces: [(name: "matte")],
                palette: [
                    (name: "red", color: "#ff0000", surface: "matte"),
                    (name: "blue", color: "#0000ff", surface: "matte"),
                ],
                ops: [{ops}],
            )"##
        )))
    }

    #[test]
    fn painted_layers_read_like_pictures_from_the_front() {
        let m = model(
            r#"
            Shape(geom: Box(min: (0, 0, 0), max: (4, 3, 2)), fill: Solid("blue")),
            Voxels(
                at: (0, 0, 0),
                legend: {'r': "red", '_': "air"},
                layers: [
                    ["r...",
                     ".. _",
                     "...r"],
                    ["", "", "rr"],
                ],
            ),
            "#,
        )
        .unwrap();
        let (red, blue) = (m.palette.find("red").unwrap(), m.palette.find("blue").unwrap());
        let at = |x, y, z| m.grid.get(IVec3::new(x, y, z));
        // The first row is the top of the layer.
        assert_eq!(at(0, 2, 0), red);
        assert_eq!(at(3, 0, 0), red);
        assert_eq!(at(3, 1, 0), AIR);
        // `.` and space leave what was there.
        assert_eq!(at(2, 1, 0), blue);
        assert_eq!(at(0, 0, 0), blue);
        // The second layer is one step back in z.
        assert_eq!((at(0, 0, 1), at(1, 0, 1), at(2, 0, 1)), (red, red, blue));
        assert_eq!(at(0, 2, 1), blue);
    }

    #[test]
    fn painting_past_the_grid_is_clipped() {
        let m = model(r#"Voxels(at: (2, 2, 1), legend: {'r': "red"}, layers: [["rrrr", "rrrr"], ["rrrr"]])"#).unwrap();
        let red = m.palette.find("red").unwrap();
        assert_eq!(m.grid.cells().iter().filter(|&&c| c == red).count(), 2);
        assert_eq!(m.grid.get(IVec3::new(3, 2, 1)), red);
    }

    #[test]
    fn painting_needs_every_key_in_the_legend() {
        let unknown = model(r#"Voxels(at: (0, 0, 0), legend: {'r': "red"}, layers: [["rx"]])"#);
        assert!(unknown.is_err_and(|e| e.contains("'x'")));
        let missing = model(r#"Voxels(at: (0, 0, 0), legend: {'r': "pink"}, layers: [["r"]])"#);
        assert!(missing.is_err_and(|e| e.contains("pink")));
        let reserved = model(r#"Voxels(at: (0, 0, 0), legend: {'.': "red"}, layers: [["."]])"#);
        assert!(reserved.is_err_and(|e| e.contains("reserved")));
    }

    #[test]
    fn the_shipped_models_build() {
        for path in ["characters/hero.vox.ron", "models/sakura_island.vox.ron"] {
            let text = std::fs::read_to_string(format!("{}/../../assets/{path}", env!("CARGO_MANIFEST_DIR"))).unwrap();
            build(parse(&text)).unwrap_or_else(|e| panic!("{path}: {e}"));
        }
    }
}
