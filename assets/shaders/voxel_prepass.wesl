// The voxel prepass: what Bevy's standard prepass writes for a voxel (depth, normals,
// motion vectors), with the camera cutout applied so the depth the main pass tests
// against has the same holes. Voxels have no normal maps, so the normal is the face's.

import bevy_pbr::{
    prepass::io::{VertexOutput, FragmentOutput},
    render::{pbr_functions, pbr_prepass_functions},
};
import super::voxel_cutout::cut;

@fragment
fn fragment(in: VertexOutput) -> FragmentOutput {
@if(VISIBILITY_RANGE_DITHER)
    pbr_functions::visibility_range_dither(in.position, in.visibility_range_dither);
@if(CAMERA_CUTOUT)
    cut(in.world_position.xyz, in.position.xy);
    pbr_prepass_functions::prepass_sample_color_and_alpha_discard(in);

    var out: FragmentOutput;
@if(UNCLIPPED_DEPTH_ORTHO_EMULATION)
    out.frag_depth = in.unclipped_depth;
@if(NORMAL_PREPASS)
    out.normal = vec4(normalize(in.world_normal) * 0.5 + vec3(0.5), 1.0);
@if(MOTION_VECTOR_PREPASS)
    out.motion_vector = pbr_prepass_functions::calculate_motion_vector(in.world_position, in.previous_world_position);
    return out;
}
