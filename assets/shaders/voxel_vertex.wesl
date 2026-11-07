// Voxel faces arrive as two packed words per vertex (see `mesher.rs`) rather than
// Bevy's full vertex layout. Everything else follows from which face and corner a
// vertex is: its normal, its tangent, its position across the face. Serves the main
// pass and the prepass (depth, normals, motion vectors, shadows), filling whichever
// fields of that pass's `VertexOutput` are present.

import bevy_pbr::render::{mesh_functions, view_transformations::position_world_to_clip};

@if(PREPASS_PIPELINE)
import bevy_pbr::prepass::io::VertexOutput;
@else
import bevy_pbr::render::forward_io::VertexOutput;

struct Vertex {
    @builtin(instance_index) instance_index: u32,
    @location(0) voxel: vec2<u32>,
}

// In the mesher's face order.
const NORMALS = array<vec3<f32>, 6>(
    vec3(1.0, 0.0, 0.0), vec3(-1.0, 0.0, 0.0),
    vec3(0.0, 1.0, 0.0), vec3(0.0, -1.0, 0.0),
    vec3(0.0, 0.0, 1.0), vec3(0.0, 0.0, -1.0),
);
// Along u, from the face's first corner to its second. Every face's bitangent,
// `cross(normal, tangent)`, points along v, so the tangent's w is always 1.
const TANGENTS = array<vec3<f32>, 6>(
    vec3(0.0, 1.0, 0.0), vec3(0.0, 0.0, 1.0),
    vec3(0.0, 0.0, 1.0), vec3(1.0, 0.0, 0.0),
    vec3(1.0, 0.0, 0.0), vec3(0.0, 1.0, 0.0),
);
const CORNER_UVS = array<vec2<f32>, 4>(vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(1.0, 1.0), vec2(0.0, 1.0));
// Brightness for 0..=3 unoccluded neighbors around a corner.
const AO_CURVE = array<f32, 4>(0.45, 0.64, 0.82, 1.0);

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    return select(pow((c + 0.055) / 1.055, vec3(2.4)), c / 12.92, c <= vec3(0.04045));
}

@vertex
fn vertex(in: Vertex) -> VertexOutput {
    // x, y, z: 6 bits each; face: 3; corner: 2; light: 2; edge flags: 4.
    let a = in.voxel.x;
    let position = vec3<f32>(vec3(a, a >> 6u, a >> 12u) & vec3(63u));
    let face = (a >> 18u) & 7u;
    let corner = (a >> 21u) & 3u;
    let light = (a >> 23u) & 3u;
    let edges = (a >> 25u) & 15u;
    // Copies, so they can be indexed at runtime.
    var normals = NORMALS;
    var tangents = TANGENTS;
    var corner_uvs = CORNER_UVS;
    var ao_curve = AO_CURVE;

    let world_from_local = mesh_functions::get_world_from_local(in.instance_index);
    var out: VertexOutput;
    out.world_position = mesh_functions::mesh_position_local_to_world(world_from_local, vec4(position, 1.0));
    out.position = position_world_to_clip(out.world_position.xyz);
@if(PREPASS_PIPELINE && UNCLIPPED_DEPTH_ORTHO_EMULATION) {
    out.unclipped_depth = out.position.z;
    out.position.z = min(out.position.z, 1.0);
}

@if(!PREPASS_PIPELINE || NORMAL_PREPASS_OR_DEFERRED_PREPASS) {
    out.world_normal = mesh_functions::mesh_normal_local_to_world(normals[face], in.instance_index);
@if(VERTEX_TANGENTS)
    out.world_tangent = mesh_functions::mesh_tangent_local_to_world(
        world_from_local,
        vec4(tangents[face], 1.0),
        in.instance_index
    );
}

@if(VERTEX_UVS_A)
    out.uv = corner_uvs[corner];
@if(VERTEX_UVS_B)
    out.uv_b = vec2(f32(edges), 0.0);
@if(VERTEX_COLORS)
    out.color = vec4(srgb_to_linear(unpack4x8unorm(in.voxel.y).rgb) * ao_curve[light], 1.0);

@if(PREPASS_PIPELINE && MOTION_VECTOR_PREPASS)
    out.previous_world_position = mesh_functions::mesh_position_local_to_world(
        mesh_functions::get_previous_world_from_local(in.instance_index),
        vec4(position, 1.0)
    );
@if(VERTEX_OUTPUT_INSTANCE_INDEX)
    out.instance_index = in.instance_index;
@if(VISIBILITY_RANGE_DITHER)
    out.visibility_range_dither = mesh_functions::get_visibility_range_dither_level(
        in.instance_index, world_from_local[3]);
    return out;
}
