//! The build spec agents answer with, and the limits every build must pass.
//!
//! The spec is deliberately smaller than the `.vox.ron` format and shaped for strict
//! JSON-schema structured output (every field present, `null` for "unused"). It is
//! compiled into a regular voxel model, then measured; the same checks are meant to
//! run on a server later, so nothing here trusts the agent.

use bevy_color::prelude::*;
use bevy_math::prelude::*;
use serde::{Deserialize, Serialize};

use crate::sculpt::TerrainSpec;
use crate::voxel::{
    ModelData,
    faces::count_visible,
    model::{Fill, Geom, ModelDef, NoiseDef, OpDef, PaletteDef, SurfaceDef, build},
};

pub const SCHEMA: &str = include_str!("build_spec.schema.json");

/// Object voxels are 12.5 cm: finer than terrain (25 cm), coarser than characters (5 cm).
pub const VOXEL_SIZE: f32 = 0.125;

/// Limits one build must pass, checked before and after rasterizing. Each server sets
/// its own ([`crate::settings::WorldRules`]); the defaults suit a shared world.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BuildLimits {
    /// Longest axis, in build voxels.
    pub max_size: u32,
    pub max_shapes: usize,
    pub max_colors: usize,
    pub max_voxels: usize,
    /// Visible faces after meshing: the real rendering cost of a build.
    pub max_faces: usize,
    pub max_glow_voxels: usize,
    pub max_glass_voxels: usize,
}

impl Default for BuildLimits {
    fn default() -> Self {
        Self {
            max_size: 64,
            max_shapes: 200,
            max_colors: 24,
            max_voxels: 120_000,
            max_faces: 60_000,
            max_glow_voxels: 3_000,
            max_glass_voxels: 20_000,
        }
    }
}

impl BuildLimits {
    /// For builds the world already accepted under the rules of their time: they keep
    /// rendering if a server tightens its limits later. Still bounded, so a broken spec
    /// can't exhaust memory.
    pub const ACCEPTED: Self = Self {
        max_size: 256,
        max_shapes: 4096,
        max_colors: 255,
        max_voxels: usize::MAX,
        max_faces: usize::MAX,
        max_glow_voxels: usize::MAX,
        max_glass_voxels: usize::MAX,
    };
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentAnswer {
    pub reply: String,
    pub build: Option<BuildSpec>,
    pub terrain: Option<TerrainSpec>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BuildSpec {
    pub name: String,
    pub size: Vec<i64>,
    pub palette: Vec<ColorSpec>,
    pub shapes: Vec<ShapeSpec>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColorSpec {
    pub name: String,
    pub hex: String,
    pub material: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShapeSpec {
    pub kind: String,
    pub color: String,
    pub from: Vec<f64>,
    pub to: Vec<f64>,
    pub radius: Option<f64>,
    pub wobble: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BuildStats {
    pub size: UVec3,
    pub voxels: usize,
    pub faces: usize,
    pub glow: usize,
}

/// A validated build, ready to spawn.
pub struct CompiledBuild {
    pub model: ModelData,
    pub stats: BuildStats,
}

fn vec3(v: &[f64], what: &str, errors: &mut Vec<String>) -> (f32, f32, f32) {
    if v.len() != 3 || v.iter().any(|c| !c.is_finite()) {
        errors.push(format!("{what} must be three finite numbers"));
        return (0.0, 0.0, 0.0);
    }
    (v[0] as f32, v[1] as f32, v[2] as f32)
}

/// Checks the spec, rasterizes it, and measures the result.
/// Errors are phrased so they can be shown to the player or fed back to the agent.
pub fn compile(spec: &BuildSpec, limits: &BuildLimits) -> Result<CompiledBuild, Vec<String>> {
    let BuildLimits { max_size, max_shapes, max_colors, max_voxels, max_faces, max_glow_voxels, max_glass_voxels } =
        *limits;
    let mut errors = Vec::new();

    if spec.size.len() != 3 || spec.size.iter().any(|&s| s < 1 || s > max_size as i64) {
        errors.push(format!("size must be [x, y, z] with each between 1 and {max_size}"));
    }
    if spec.shapes.is_empty() || spec.shapes.len() > max_shapes {
        errors.push(format!("use between 1 and {max_shapes} shapes (got {})", spec.shapes.len()));
    }
    if spec.palette.is_empty() || spec.palette.len() > max_colors {
        errors.push(format!("use between 1 and {max_colors} colors (got {})", spec.palette.len()));
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let size = UVec3::new(spec.size[0] as u32, spec.size[1] as u32, spec.size[2] as u32);

    let mut surfaces = vec![
        SurfaceDef { name: "matte".into(), roughness: 0.9, metallic: 0.0, reflectance: 0.35, emissive: None, alpha: 1.0 },
        SurfaceDef { name: "glossy".into(), roughness: 0.25, metallic: 0.0, reflectance: 0.5, emissive: None, alpha: 1.0 },
        SurfaceDef { name: "metal".into(), roughness: 0.35, metallic: 0.9, reflectance: 0.5, emissive: None, alpha: 1.0 },
        SurfaceDef { name: "glass".into(), roughness: 0.05, metallic: 0.0, reflectance: 0.6, emissive: None, alpha: 0.45 },
    ];
    let mut palette = Vec::new();
    for (i, c) in spec.palette.iter().enumerate() {
        if c.name == "air" || spec.palette[..i].iter().any(|o| o.name == c.name) {
            errors.push(format!("color name {:?} is reserved or repeated", c.name));
        }
        if Srgba::hex(&c.hex).is_err() {
            errors.push(format!("color {:?} has invalid hex {:?}", c.name, c.hex));
            continue;
        }
        let surface = match c.material.as_str() {
            "matte" | "glossy" | "metal" | "glass" => c.material.clone(),
            "glow" => {
                // Emission is per surface, so each glowing color gets its own.
                let name = format!("glow_{i}");
                surfaces.push(SurfaceDef {
                    name: name.clone(),
                    roughness: 0.5,
                    metallic: 0.0,
                    reflectance: 0.4,
                    emissive: Some((c.hex.clone(), 6_000.0)),
                    alpha: 1.0,
                });
                name
            }
            other => {
                errors.push(format!("color {:?} has unknown material {other:?}", c.name));
                continue;
            }
        };
        palette.push(PaletteDef { name: c.name.clone(), color: c.hex.clone(), surface, variation: 0.04 });
    }

    let mut ops = Vec::new();
    let slack = 2.0;
    let bounds = size.as_vec3() + slack;
    for (i, s) in spec.shapes.iter().enumerate() {
        let what = format!("shape {i} ({})", s.kind);
        let from = vec3(&s.from, &format!("{what} from"), &mut errors);
        let to = vec3(&s.to, &format!("{what} to"), &mut errors);
        let (a, b) = (Vec3::from(from), Vec3::from(to));
        if a.min(b).cmplt(Vec3::splat(-slack)).any() || a.max(b).cmpgt(bounds).any() {
            errors.push(format!("{what} reaches outside the build volume {size}"));
        }
        if s.color != "air" && !spec.palette.iter().any(|c| c.name == s.color) {
            errors.push(format!("{what} uses unknown color {:?}", s.color));
        }
        let (lo, hi) = (a.min(b), a.max(b));
        let geom = match s.kind.as_str() {
            "box" => Geom::Box { min: lo.into(), max: hi.into() },
            "ellipsoid" => Geom::Ellipsoid {
                center: ((lo + hi) / 2.0).into(),
                radii: ((hi - lo) / 2.0).max(Vec3::splat(0.5)).into(),
            },
            "cylinder" => {
                let c = (lo + hi) / 2.0;
                Geom::Cylinder {
                    base: (c.x, lo.y, c.z),
                    radius: ((hi.x - lo.x).min(hi.z - lo.z) / 2.0).max(0.5),
                    height: hi.y - lo.y,
                }
            }
            "capsule" => {
                let r = s.radius.unwrap_or(1.0) as f32;
                if !(0.5..=16.0).contains(&r) {
                    errors.push(format!("{what} radius must be between 0.5 and 16"));
                }
                Geom::Capsule { a: from, b: to, radius_a: r, radius_b: r }
            }
            other => {
                errors.push(format!("{what} has unknown kind {other:?}"));
                continue;
            }
        };
        let wobble = s.wobble.unwrap_or(0.0).clamp(0.0, 0.4) as f32;
        ops.push(OpDef::Shape {
            geom,
            fill: Fill::Solid(s.color.clone()),
            noise: (wobble > 0.0).then_some(NoiseDef { scale: 3.0, amplitude: wobble, seed: i as u32 }),
            mask: None,
        });
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    let def = ModelDef {
        voxel_size: VOXEL_SIZE,
        size: (size.x, size.y, size.z),
        pivot: (size.x as f32 / 2.0, 0.0, size.z as f32 / 2.0),
        surfaces,
        palette,
        ops,
        lights: Vec::new(),
        parts: Vec::new(),
    };
    let model = build(def).map_err(|e| vec![e])?;

    let mut stats = BuildStats { size, ..Default::default() };
    let (mut glass, grid) = (0, &model.grid);
    for z in 0..size.z as i32 {
        for y in 0..size.y as i32 {
            for x in 0..size.x as i32 {
                let id = grid.get(IVec3::new(x, y, z));
                if id == crate::voxel::AIR {
                    continue;
                }
                stats.voxels += 1;
                let surface = model.palette.surface_of(id);
                if surface.emissive != LinearRgba::BLACK {
                    stats.glow += 1;
                }
                if surface.is_transparent() {
                    glass += 1;
                }
            }
        }
    }
    stats.faces = count_visible(grid, &model.palette);

    if stats.voxels == 0 {
        errors.push("the build is empty".into());
    }
    if stats.voxels > max_voxels {
        errors.push(format!("{} voxels is over the limit of {max_voxels}; hollow it out or shrink it", stats.voxels));
    }
    if stats.faces > max_faces {
        errors.push(format!("{} visible faces is over the limit of {max_faces}; use fewer, larger shapes", stats.faces));
    }
    if stats.glow > max_glow_voxels {
        errors.push(format!("{} glowing voxels is over the limit of {max_glow_voxels}", stats.glow));
    }
    if glass > max_glass_voxels {
        errors.push(format!("{glass} glass voxels is over the limit of {max_glass_voxels}"));
    }
    if errors.is_empty() { Ok(CompiledBuild { model, stats }) } else { Err(errors) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(kind: &str, color: &str, from: [f64; 3], to: [f64; 3]) -> ShapeSpec {
        ShapeSpec { kind: kind.into(), color: color.into(), from: from.into(), to: to.into(), radius: None, wobble: None }
    }

    fn lamp() -> BuildSpec {
        BuildSpec {
            name: "Lamp post".into(),
            size: vec![7, 22, 7],
            palette: vec![
                ColorSpec { name: "iron".into(), hex: "#3d4248".into(), material: "metal".into() },
                ColorSpec { name: "light".into(), hex: "#ffd88a".into(), material: "glow".into() },
            ],
            shapes: vec![
                shape("box", "iron", [1.0, 0.0, 1.0], [6.0, 2.0, 6.0]),
                shape("cylinder", "iron", [2.0, 2.0, 2.0], [5.0, 17.0, 5.0]),
                shape("box", "light", [1.0, 17.0, 1.0], [6.0, 21.0, 6.0]),
                shape("box", "iron", [0.0, 21.0, 0.0], [7.0, 22.0, 7.0]),
            ],
        }
    }

    #[test]
    fn prompt_example_compiles() {
        let build = compile(&lamp(), &BuildLimits::default()).expect("the example in the system prompt must be valid");
        assert_eq!(build.stats.size, UVec3::new(7, 22, 7));
        assert_eq!(build.stats.glow, 5 * 4 * 5);
        assert!(build.stats.faces > 0);
    }

    #[test]
    fn rejects_unknown_colors_and_oversize() {
        let mut spec = lamp();
        spec.size = vec![7, 200, 7];
        assert!(compile(&spec, &BuildLimits::default()).is_err());
        let mut spec = lamp();
        spec.shapes[0].color = "gold".into();
        let errors = compile(&spec, &BuildLimits::default()).err().unwrap();
        assert!(errors[0].contains("unknown color"));
    }

    #[test]
    fn rejects_dense_builds() {
        let mut spec = lamp();
        spec.size = vec![64, 64, 64];
        spec.shapes = vec![shape("box", "iron", [0.0, 0.0, 0.0], [64.0, 64.0, 64.0])];
        let errors = compile(&spec, &BuildLimits::default()).err().unwrap();
        assert!(errors.iter().any(|e| e.contains("voxels is over the limit")));
    }

    #[test]
    fn schema_is_valid_json() {
        let schema: serde_json::Value = serde_json::from_str(SCHEMA).unwrap();
        assert_eq!(schema["required"][1], "build");
    }
}
