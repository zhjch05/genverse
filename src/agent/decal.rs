//! Decals: what games draw on the ground under a piece with a `decal` component, with
//! a shader of their own (`genverse_rules::logic::shader`).
//!
//! Each is a flat square at the piece's feet. Its color comes from the package's WGSL
//! `fn decal`, which the game wraps into a whole fragment shader and hands to the GPU as
//! it runs: no file, no restart. A new version of the package brings new source, which
//! is a new shader and pipeline, so a changed shader shows the moment it installs, for
//! every player. Pieces with the same source share one shader.

use std::collections::HashMap;

use bevy::{
    light::{NotShadowCaster, NotShadowReceiver},
    mesh::MeshVertexBufferLayoutRef,
    pbr::{MaterialPipeline, MaterialPipelineKey},
    prelude::*,
    render::render_resource::{
        AsBindGroup, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError,
    },
    shader::ShaderRef,
};
use genverse_rules::logic::{
    EntityId, Logic,
    shader::{DECAL_PRELUDE, DEFAULT_DECAL, DecalLook},
    walk::walking_at,
};

use crate::world::ledger::Ledger;

/// How far above the piece's feet a decal floats, so it never sinks into the ground.
const LIFT: f32 = 0.03;

/// Draws a square with a package's `fn decal`.
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
#[bind_group_data(DecalKey)]
pub struct DecalMaterial {
    #[uniform(0)]
    uniform: DecalUniform,
    /// The whole fragment shader, made from the package's source.
    shader: Handle<Shader>,
}

#[derive(ShaderType, Debug, Clone)]
struct DecalUniform {
    color: Vec4,
    params: Vec4,
    /// x: meters across.
    size: Vec4,
}

/// Which shader a decal's pipeline is built with.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct DecalKey(Handle<Shader>);

impl From<&DecalMaterial> for DecalKey {
    fn from(material: &DecalMaterial) -> Self {
        DecalKey(material.shader.clone())
    }
}

impl Material for DecalMaterial {
    fn fragment_shader() -> ShaderRef {
        // Replaced per shader in `specialize`.
        ShaderRef::Default
    }

    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Blend
    }

    fn specialize(
        _pipeline: &MaterialPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        _layout: &MeshVertexBufferLayoutRef,
        key: MaterialPipelineKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        if let Some(fragment) = &mut descriptor.fragment {
            fragment.shader = key.bind_group_data.0.clone();
        }
        // On the ground, which it lies on: drawn over it rather than fighting it.
        if let Some(depth) = &mut descriptor.depth_stencil {
            depth.bias.constant = 4;
            depth.bias.slope_scale = 2.0;
        }
        Ok(())
    }
}

/// A package's `fn decal` as a whole fragment shader.
fn fragment_source(decal: &str) -> String {
    format!(
        "import bevy_pbr::render::{{forward_io::VertexOutput, mesh_view_bindings::globals}};

struct GenverseDecal {{
    color: vec4<f32>,
    params: vec4<f32>,
    size: vec4<f32>,
}}

@group(constants::MATERIAL_BIND_GROUP) @binding(0)
var<uniform> genverse_decal: GenverseDecal;

{decal}
{DECAL_PRELUDE}
@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {{
    let uv = in.uv * 2.0 - 1.0;
    let c = decal(Decal(uv, genverse_decal.size.x, globals.time, genverse_decal.color, genverse_decal.params));
    return vec4(max(c.rgb, vec3(0.0)), clamp(c.a, 0.0, 1.0));
}}
"
    )
}

/// The decals drawn, and the shaders they're drawn with.
#[derive(Resource, Default)]
pub struct Decals {
    /// Each piece's decal: what it is, and the entity drawing it.
    drawn: HashMap<EntityId, (DecalLook, String, Entity)>,
    /// Shaders by the package source they were made from.
    shaders: HashMap<String, Handle<Shader>>,
    square: Option<Handle<Mesh>>,
}

/// The source a piece's decal draws with, if it has one: the package's own shader, or the
/// default ring.
fn decal_of(logic: &Logic, e: &genverse_rules::logic::Entity) -> Option<(DecalLook, String)> {
    let package = logic
        .installed
        .get(&e.package)
        .and_then(|i| logic.package(i))?;
    let value = e.components.get("decal")?;
    let names: Vec<String> = package.shaders.keys().cloned().collect();
    let look = DecalLook::read(value, &names)?;
    let source = match &look.shader {
        Some(name) => package.shaders.get(name)?.clone(),
        None => DEFAULT_DECAL.to_string(),
    };
    Some((look, source))
}

/// Keeps a decal under every piece that has one, made again when its decal or shader
/// changes, and following the piece as it walks.
pub fn sync_decals(
    mut commands: Commands,
    ledger: Res<Ledger>,
    mut decals: ResMut<Decals>,
    mut shaders: ResMut<Assets<Shader>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<DecalMaterial>>,
    mut transforms: Query<&mut Transform>,
    mut seen: Local<Option<u64>>,
) {
    let logic = &ledger.logic;
    let decals = &mut *decals;
    let now = super::game::now_exact();
    if *seen != Some(ledger.version) {
        *seen = Some(ledger.version);
        let square = decals
            .square
            .get_or_insert_with(|| meshes.add(Plane3d::new(Vec3::Y, Vec2::splat(0.5))))
            .clone();
        let mut keep = HashMap::new();
        for (id, e) in logic.state.entities() {
            let Some((look, source)) = decal_of(logic, e) else {
                continue;
            };
            if let Some((old, old_source, entity)) = decals.drawn.remove(&id) {
                if old == look && old_source == source {
                    keep.insert(id, (look, source, entity));
                    continue;
                }
                commands.entity(entity).despawn();
            }
            let shader = decals
                .shaders
                .entry(source.clone())
                .or_insert_with(|| {
                    let n = shaders.len();
                    shaders.add(Shader::from_wesl(
                        fragment_source(&source),
                        format!("genverse://decal/{n}.wesl"),
                    ))
                })
                .clone();
            let color = Srgba::hex(look.color.trim_start_matches('#'))
                .unwrap_or(Srgba::WHITE)
                .with_alpha(look.alpha);
            let material = materials.add(DecalMaterial {
                uniform: DecalUniform {
                    color: LinearRgba::from(color).to_vec4(),
                    params: Vec4::from_array(look.params),
                    size: Vec4::new(look.size, 0.0, 0.0, 0.0),
                },
                shader,
            });
            let entity = commands
                .spawn((
                    Name::new("Decal"),
                    Mesh3d(square.clone()),
                    MeshMaterial3d(material),
                    Transform::from_translation(Vec3::from_array(e.position) + Vec3::Y * LIFT)
                        .with_scale(Vec3::splat(look.size)),
                    NotShadowCaster,
                    NotShadowReceiver,
                ))
                .id();
            keep.insert(id, (look, source, entity));
        }
        for (_, (_, _, entity)) in decals.drawn.drain() {
            commands.entity(entity).despawn();
        }
        decals.drawn = keep;
        // Shaders no decal draws with any more go, so an edited shader's old versions don't pile up.
        let used: std::collections::HashSet<&String> =
            decals.drawn.values().map(|(_, source, _)| source).collect();
        decals.shaders.retain(|source, _| used.contains(source));
    }
    for (id, (_, _, entity)) in &decals.drawn {
        let (Some(e), Ok(mut transform)) = (logic.state.get(*id), transforms.get_mut(*entity))
        else {
            continue;
        };
        let at = walking_at(e, now).map_or(e.position, |(at, _)| at);
        transform.translation = Vec3::from_array(at) + Vec3::Y * LIFT;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the game hands the GPU is a shader the compiler takes, once Bevy's imports are
    /// filled in: checked here with stand-ins for them.
    #[test]
    fn the_wrapped_shader_compiles() {
        let source = fragment_source(DEFAULT_DECAL)
            .replace(
                "import bevy_pbr::render::{forward_io::VertexOutput, mesh_view_bindings::globals};",
                "struct VertexOutput { @location(2) uv: vec2<f32> }\nstruct Globals { time: f32 }\n@group(0) @binding(11) var<uniform> globals: Globals;",
            )
            .replace("constants::MATERIAL_BIND_GROUP", "3");
        genverse_rules::logic::shader::compile(&source).unwrap_or_else(|e| panic!("{e}"));
    }

    /// The example in the agent's reference compiles as written.
    #[test]
    fn the_documented_example_compiles() {
        let doc = super::super::prompt::GAME_DOC;
        let start = doc
            .find("    // A golden ring")
            .expect("the reference has the example");
        let lines: Vec<&str> = doc[start..].lines().collect();
        let end = lines
            .iter()
            .position(|l| *l == "    }")
            .expect("the example ends");
        let example: String = lines[..=end]
            .iter()
            .map(|l| format!("{}\n", l.strip_prefix("    ").unwrap_or(l)))
            .collect();
        genverse_rules::logic::shader::check_decal("example", &example).unwrap();
    }
}
