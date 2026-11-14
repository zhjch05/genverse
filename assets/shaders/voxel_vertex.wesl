// Voxel faces arrive as two packed words per vertex (see `mesher.rs`) rather than
// Bevy's full vertex layout. Everything else follows from which face and corner a
// vertex is: its normal, its tangent, its position across the face. Serves the main
// pass and the prepass (depth, normals, motion vectors, shadows), filling whichever
// fields of that pass's `VertexOutput` are present.
//
// It also plays reveals (`reveal.rs`): a voxel with an order grows from nothing, around
// its own middle, when the clock of the reveal its mesh is tagged with reaches it.

import bevy_pbr::render::{mesh_functions, view_transformations::position_world_to_clip};

@if(PREPASS_PIPELINE)
import bevy_pbr::prepass::io::VertexOutput;
@else
import bevy_pbr::render::forward_io::VertexOutput;

struct Vertex {
    @builtin(instance_index) instance_index: u32,
    @location(0) voxel: vec2<u32>,
}

struct VoxelReveal {
    // x: seconds on the reveals' clock; y: a frame ago, for motion vectors.
    clock: vec4<f32>,
    // Per reveal, x: when it started; y: how long it takes (0: it's over).
    slots: array<vec4<f32>, 63>,
}

@group(constants::MATERIAL_BIND_GROUP) @binding(102)
var<storage, read> reveal: VoxelReveal;

// The share of a reveal one voxel takes to grow in.
const POP: f32 = 0.12;
// How long a voxel that just arrived glows on, in pops.
const GLOW: f32 = 2.5;

// How far past its arrival a voxel of order `at` (0..1) is at `now`, in pops: under 0
// it isn't there yet, over 1 it's whole.
fn since(now: f32, slot: vec4<f32>, at: f32) -> f32 {
    if slot.y <= 0.0 {
        return 1000.0;
    }
    return ((now - slot.x) / slot.y * (1.0 + POP) - at) / POP;
}

// A voxel's size (0..1) `t` pops past its arrival. Voxels that swap change at once;
// in a ghost (`going`) everything runs the other way.
fn size_at(t: f32, swap: bool, going: bool) -> f32 {
    let left = 1.0 - saturate(t);
    let grown = select(1.0 - left * left * left, step(0.0, t), swap);
    return select(grown, 1.0 - grown, going);
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
    let swap = ((a >> 29u) & 1u) == 1u;
    let arrival = in.voxel.y >> 24u;
    // Copies, so they can be indexed at runtime.
    var normals = NORMALS;
    var tangents = TANGENTS;
    var corner_uvs = CORNER_UVS;
    var ao_curve = AO_CURVE;

    // x: the voxel's size now; y: a frame ago; z: how freshly it arrived.
    var revealed = vec3(1.0, 1.0, 0.0);
    let tag = mesh_functions::get_tag(in.instance_index);
    if tag != 0u && arrival != 0u {
        let going = (tag & 0x80000000u) != 0u;
        let slot = reveal.slots[(tag & 0xffu) - 1u];
        let at = f32(arrival) / 255.0;
        let t = since(reveal.clock.x, slot, at);
        revealed.x = size_at(t, swap, going);
        revealed.y = size_at(since(reveal.clock.y, slot, at), swap, going);
        if !going && t > 0.0 {
            revealed.z = saturate(1.0 - t / GLOW);
        }
    }
    // The middle of the voxel this corner belongs to: half a voxel back from the face,
    // and from the corner to the middle of the face.
    let uv = corner_uvs[corner];
    let bitangent = cross(normals[face], tangents[face]);
    let middle = position - normals[face] * 0.5 + tangents[face] * (0.5 - uv.x) + bitangent * (0.5 - uv.y);
    let shown = middle + (position - middle) * revealed.x;

    let world_from_local = mesh_functions::get_world_from_local(in.instance_index);
    var out: VertexOutput;
    out.world_position = mesh_functions::mesh_position_local_to_world(world_from_local, vec4(shown, 1.0));
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
    out.uv_b = vec2(f32(edges), revealed.z);
@if(VERTEX_COLORS)
    out.color = vec4(srgb_to_linear(unpack4x8unorm(in.voxel.y).rgb) * ao_curve[light], 1.0);

@if(PREPASS_PIPELINE && MOTION_VECTOR_PREPASS)
    out.previous_world_position = mesh_functions::mesh_position_local_to_world(
        mesh_functions::get_previous_world_from_local(in.instance_index),
        vec4(middle + (position - middle) * revealed.y, 1.0)
    );
@if(VERTEX_OUTPUT_INSTANCE_INDEX)
    out.instance_index = in.instance_index;
@if(VISIBILITY_RANGE_DITHER)
    out.visibility_range_dither = mesh_functions::get_visibility_range_dither_level(
        in.instance_index, world_from_local[3]);
    return out;
}
