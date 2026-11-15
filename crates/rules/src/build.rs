//! The build spec agents answer with, and the limits every build must pass.
//!
//! The spec is deliberately smaller than the `.vox.ron` format and shaped for strict
//! JSON-schema structured output (every field present, `null` for "unused"). It is
//! compiled into a regular voxel model, then measured; the same checks are meant to
//! run on a server later, so nothing here trusts the agent.

use bevy_color::prelude::*;
use bevy_math::prelude::*;
use serde::{Deserialize, Serialize};

use crate::content::{self, Tune};
use crate::voxel::{
    ModelData,
    faces::count_visible,
    model::{Fill, Geom, ModelDef, NoiseDef, OpDef, PaletteDef, SurfaceDef, build_ordered},
};
use crate::{logic::data::GameSpec, sculpt::TerrainSpec};

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
    /// Game rules for the selected area, as a data package.
    #[serde(default)]
    pub game: Option<Box<GameSpec>>,
    /// Makes the build a portal to this world (by name) or server (`host:port`).
    #[serde(default)]
    pub portal: Option<String>,
    /// What things in the world mean ([`crate::semantic`]).
    #[serde(default)]
    pub describe: Option<Vec<crate::semantic::Meaning>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BuildSpec {
    pub name: String,
    pub size: Vec<i64>,
    pub palette: Vec<ColorSpec>,
    pub shapes: Vec<ShapeSpec>,
    /// What it is and what it's for, in a line ([`crate::semantic`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColorSpec {
    pub name: String,
    pub hex: String,
    pub material: String,
    /// What this color changes about its material ([`crate::content`]).
    #[serde(flatten)]
    pub tune: Tune,
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
    compile_ordered(spec, limits, None)
}

/// Which shape of `spec` made each voxel of its model: the index of the last shape to
/// change the cell, in the grid's layout. For showing a build arrive in the order it
/// was written. `None` if the spec doesn't compile.
pub fn shape_order(spec: &BuildSpec) -> Option<Vec<u16>> {
    let mut order = Vec::new();
    compile_ordered(spec, &BuildLimits::ACCEPTED, Some(&mut order)).ok()?;
    Some(order)
}

fn compile_ordered(
    spec: &BuildSpec,
    limits: &BuildLimits,
    order: Option<&mut Vec<u16>>,
) -> Result<CompiledBuild, Vec<String>> {
    let BuildLimits {
        max_size,
        max_shapes,
        max_colors,
        max_voxels,
        max_faces,
        max_glow_voxels,
        max_glass_voxels,
    } = *limits;
    let mut errors = Vec::new();

    if spec.size.len() != 3 || spec.size.iter().any(|&s| s < 1 || s > max_size as i64) {
        errors.push(format!(
            "size must be [x, y, z] with each between 1 and {max_size}"
        ));
    }
    if spec.shapes.is_empty() || spec.shapes.len() > max_shapes {
        errors.push(format!(
            "use between 1 and {max_shapes} shapes (got {})",
            spec.shapes.len()
        ));
    }
    if spec.palette.is_empty() || spec.palette.len() > max_colors {
        errors.push(format!(
            "use between 1 and {max_colors} colors (got {})",
            spec.palette.len()
        ));
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let size = UVec3::new(
        spec.size[0] as u32,
        spec.size[1] as u32,
        spec.size[2] as u32,
    );

    // The plain materials, shared by every color that doesn't change them.
    let mut surfaces: Vec<SurfaceDef> = content::MATERIALS
        .iter()
        .filter(|(_, finish, _)| finish.glow == 0.0)
        .map(|(name, f, _)| SurfaceDef {
            name: name.to_string(),
            roughness: f.roughness,
            metallic: f.metallic,
            reflectance: f.reflectance,
            emissive: None,
            alpha: f.alpha,
        })
        .collect();
    let mut palette = Vec::new();
    for (i, c) in spec.palette.iter().enumerate() {
        if c.name == "air" || spec.palette[..i].iter().any(|o| o.name == c.name) {
            errors.push(format!("color name {:?} is reserved or repeated", c.name));
        }
        if Srgba::hex(&c.hex).is_err() {
            errors.push(format!("color {:?} has invalid hex {:?}", c.name, c.hex));
            continue;
        }
        let finish = match content::check(&c.material, &c.tune) {
            Ok(finish) => finish,
            Err(e) => {
                errors.push(format!("color {:?}: {e}", c.name));
                continue;
            }
        };
        let surface = if c.tune.is_plain() && finish.glow == 0.0 {
            c.material.clone()
        } else {
            // Emission is per surface, and so is a color's own finish: each gets its own.
            let name = format!("{}_{i}", if c.tune.is_plain() { "glow" } else { "own" });
            surfaces.push(SurfaceDef {
                name: name.clone(),
                roughness: finish.roughness,
                metallic: finish.metallic,
                reflectance: finish.reflectance,
                emissive: (finish.glow > 0.0).then(|| (c.hex.clone(), finish.light())),
                alpha: finish.alpha,
            });
            name
        };
        palette.push(PaletteDef {
            name: c.name.clone(),
            color: c.hex.clone(),
            surface,
            variation: 0.04,
        });
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
            "box" => Geom::Box {
                min: lo.into(),
                max: hi.into(),
            },
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
                Geom::Capsule {
                    a: from,
                    b: to,
                    radius_a: r,
                    radius_b: r,
                }
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
            noise: (wobble > 0.0).then_some(NoiseDef {
                scale: 3.0,
                amplitude: wobble,
                seed: i as u32,
            }),
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
    // One op per shape, in the spec's order, so an op's index is its shape's.
    let model = build_ordered(def, order).map_err(|e| vec![e])?;

    let mut stats = BuildStats {
        size,
        ..Default::default()
    };
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
        errors.push(format!(
            "{} voxels is over the limit of {max_voxels}; hollow it out or shrink it",
            stats.voxels
        ));
    }
    if stats.faces > max_faces {
        errors.push(format!(
            "{} visible faces is over the limit of {max_faces}; use fewer, larger shapes",
            stats.faces
        ));
    }
    if stats.glow > max_glow_voxels {
        errors.push(format!(
            "{} glowing voxels is over the limit of {max_glow_voxels}",
            stats.glow
        ));
    }
    if glass > max_glass_voxels {
        errors.push(format!(
            "{glass} glass voxels is over the limit of {max_glass_voxels}"
        ));
    }
    if errors.is_empty() {
        Ok(CompiledBuild { model, stats })
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(kind: &str, color: &str, from: [f64; 3], to: [f64; 3]) -> ShapeSpec {
        ShapeSpec {
            kind: kind.into(),
            color: color.into(),
            from: from.into(),
            to: to.into(),
            radius: None,
            wobble: None,
        }
    }

    fn lamp() -> BuildSpec {
        BuildSpec {
            name: "Lamp post".into(),
            size: vec![7, 22, 7],
            palette: vec![
                ColorSpec {
                    name: "iron".into(),
                    hex: "#3d4248".into(),
                    material: "metal".into(),
                    tune: Tune::default(),
                },
                ColorSpec {
                    name: "light".into(),
                    hex: "#ffd88a".into(),
                    material: "glow".into(),
                    tune: Tune::default(),
                },
            ],
            shapes: vec![
                shape("box", "iron", [1.0, 0.0, 1.0], [6.0, 2.0, 6.0]),
                shape("cylinder", "iron", [2.0, 2.0, 2.0], [5.0, 17.0, 5.0]),
                shape("box", "light", [1.0, 17.0, 1.0], [6.0, 21.0, 6.0]),
                shape("box", "iron", [0.0, 21.0, 0.0], [7.0, 22.0, 7.0]),
            ],
            about: None,
        }
    }

    #[test]
    fn prompt_example_compiles() {
        let build = compile(&lamp(), &BuildLimits::default())
            .expect("the example in the system prompt must be valid");
        assert_eq!(build.stats.size, UVec3::new(7, 22, 7));
        assert_eq!(build.stats.glow, 5 * 4 * 5);
        assert!(build.stats.faces > 0);
    }

    #[test]
    fn shape_order_names_the_last_shape_to_touch_a_voxel() {
        let order = shape_order(&lamp()).unwrap();
        let at = |x: u32, y: u32, z: u32| order[(x + 7 * (y + 22 * z)) as usize];
        // The base, the pole, the lamp and the cap, from the ground up.
        assert_eq!(
            [at(3, 0, 3), at(3, 10, 3), at(3, 18, 3), at(3, 21, 3)],
            [0, 1, 2, 3]
        );
        assert_eq!(order.len(), 7 * 22 * 7);
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
    fn a_color_tunes_its_material() {
        let mut spec = lamp();
        // Iron that glows a little, and a light that's see-through.
        spec.palette[0].tune = Tune {
            glow: Some(0.5),
            ..Default::default()
        };
        spec.palette[1].tune = Tune {
            alpha: Some(0.5),
            ..Default::default()
        };
        let build = compile(&spec, &BuildLimits::default()).expect("in range");
        let palette = &build.model.palette;
        let (iron, light) = (
            palette.find("iron").unwrap(),
            palette.find("light").unwrap(),
        );
        assert_ne!(palette.surface_of(iron).emissive, LinearRgba::BLACK);
        assert_eq!(
            (
                palette.surface_of(iron).metallic,
                palette.surface_of(light).alpha
            ),
            (0.9, 0.5)
        );
        assert!(build.stats.glow > 5 * 4 * 5);
        spec.palette[0].tune.roughness = Some(3.0);
        assert!(
            compile(&spec, &BuildLimits::default()).err().unwrap()[0]
                .contains("roughness is from 0 to 1")
        );
        // A plain color is written, and read, as before.
        let plain = serde_json::to_string(&lamp().palette[0]).unwrap();
        assert_eq!(
            plain,
            r##"{"name":"iron","hex":"#3d4248","material":"metal"}"##
        );
        let nulls: ColorSpec = serde_json::from_str(r##"{"name":"iron","hex":"#3d4248","material":"metal","glow":null,"alpha":0.5,"roughness":null,"metallic":null}"##).unwrap();
        assert_eq!(nulls.tune.alpha, Some(0.5));
    }

    #[test]
    fn rejects_dense_builds() {
        let mut spec = lamp();
        spec.size = vec![64, 64, 64];
        spec.shapes = vec![shape("box", "iron", [0.0, 0.0, 0.0], [64.0, 64.0, 64.0])];
        let errors = compile(&spec, &BuildLimits::default()).err().unwrap();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("voxels is over the limit"))
        );
    }

    /// The answer schema is what agents are held to, so it must list what the rules
    /// take: a material, a land change or a thing to scatter added to the rules without
    /// the schema could never be asked for.
    #[test]
    fn the_schema_lists_what_the_rules_take() {
        let schema: serde_json::Value = serde_json::from_str(SCHEMA).unwrap();
        let listed = |pointer: &str| -> Vec<String> {
            let list = schema
                .pointer(pointer)
                .and_then(|v| v.as_array())
                .unwrap_or_else(|| panic!("the schema has no list at {pointer}"));
            list.iter()
                .map(|v| v.as_str().unwrap_or_default().to_string())
                .collect()
        };
        assert_eq!(
            listed("/$defs/color/properties/material/enum"),
            content::MATERIALS.map(|(name, ..)| name)
        );
        let kinds: Vec<&str> = crate::sculpt::OP_KINDS
            .into_iter()
            .chain([crate::sculpt::CLEAR])
            .collect();
        assert_eq!(listed("/$defs/terrain_op/properties/kind/enum"), kinds);
        assert_eq!(
            listed("/$defs/terrain_op/properties/item/anyOf/0/enum"),
            crate::sculpt::ITEMS
        );
        // Every shape the schema offers is one the compiler makes.
        for kind in listed("/$defs/shape/properties/kind/enum") {
            let mut spec = lamp();
            spec.shapes = vec![ShapeSpec {
                radius: Some(1.0),
                ..shape(&kind, "iron", [1.0, 1.0, 1.0], [5.0, 9.0, 5.0])
            }];
            assert!(
                compile(&spec, &BuildLimits::default()).is_ok(),
                "{kind} is in the schema and doesn't compile"
            );
        }
        // And every number a color can tune is one the schema lets it give.
        let color = schema
            .pointer("/$defs/color/properties")
            .and_then(|v| v.as_object())
            .unwrap();
        let tuned = serde_json::to_value(Tune {
            roughness: Some(0.5),
            metallic: Some(0.5),
            glow: Some(0.5),
            alpha: Some(0.5),
        })
        .unwrap();
        for field in tuned.as_object().unwrap().keys() {
            assert!(
                color.contains_key(field),
                "the schema's colors have no {field}"
            );
        }
    }

    #[test]
    fn schema_is_valid_json() {
        let schema: serde_json::Value = serde_json::from_str(SCHEMA).unwrap();
        assert_eq!(schema["required"][1], "build");
    }
}
