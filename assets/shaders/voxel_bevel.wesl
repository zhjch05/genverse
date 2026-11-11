// Voxels with softly rounded outside edges. The mesh stays made of flat cubes; near an
// edge that turns away from the face (the mesher flags those per face), the surface
// normal leans over the edge the way a small fillet would, so light rolls around it.

import bevy_pbr::render::{
    pbr_fragment::pbr_input_from_standard_material,
    pbr_functions::alpha_discard,
};
import super::voxel_cutout::cut;

@if(PREPASS_PIPELINE)
import bevy_pbr::{
    prepass::io::{VertexOutput, FragmentOutput},
    deferred::functions::deferred_output,
};
@else
import bevy_pbr::render::{
    forward_io::{VertexOutput, FragmentOutput},
    pbr_functions::{apply_pbr_lighting, main_pass_post_lighting_processing},
};

struct VoxelBevel {
    // x: fillet width as a share of a voxel (0 turns it off).
    settings: vec4<f32>,
}

@group(constants::MATERIAL_BIND_GROUP) @binding(100)
var<uniform> bevel: VoxelBevel;

// How far the normal leans toward an edge, as the tangent of the angle, at distance `d`
// (in voxels) from it. A circular fillet split between the two faces: flat where it
// starts, 45 degrees right at the edge.
fn lean(d: f32, edge: u32) -> f32 {
    let width = bevel.settings.x;
    if edge == 0u || width <= 0.0 {
        return 0.0;
    }
    let t = saturate(1.0 - d / width);
    return tan(asin(t * 0.70710678));
}

// UV 0 is the position across the face (0..1 on both axes), UV 1's x the edge flags:
// bit 0 for u = 0, 1 for u = 1, 2 for v = 0, 3 for v = 1. The tangent points along u.
fn rounded_normal(in: VertexOutput, n: vec3<f32>) -> vec3<f32> {
    let edges = u32(in.uv_b.x + 0.5);
    let t = normalize(in.world_tangent.xyz);
    let b = cross(n, t) * in.world_tangent.w;
    var bent = n;
    bent -= t * lean(in.uv.x, edges & 1u);
    bent += t * lean(1.0 - in.uv.x, edges & 2u);
    bent -= b * lean(in.uv.y, edges & 4u);
    bent += b * lean(1.0 - in.uv.y, edges & 8u);
    return normalize(bent);
}

@fragment
fn fragment(
    in: VertexOutput,
    @builtin(front_facing) is_front: bool,
) -> FragmentOutput {
@if(CAMERA_CUTOUT)
    cut(in.world_position.xyz, in.position.xy);
    var pbr_input = pbr_input_from_standard_material(in, is_front);
    pbr_input.N = rounded_normal(in, pbr_input.N);
    pbr_input.material.base_color = alpha_discard(
        pbr_input.material.flags,
        pbr_input.material.alpha_cutoff,
        pbr_input.material.base_color
    );

    var out: FragmentOutput;
@if(PREPASS_PIPELINE) {
    out = deferred_output(in, pbr_input);
} @else {
    out.color = apply_pbr_lighting(pbr_input);
    out.color = main_pass_post_lighting_processing(pbr_input, out.color);
}
    return out;
}
