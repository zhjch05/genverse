// Fades whatever stands between the camera and the hero, by dithering it away. The
// camera only moves in when most of the hero would be hidden (see `clearance` in
// `player.rs`); everything less, a branch, a post, a canopy's edge, is cut through here.
//
// The cut is a cone from the camera to the hero, so it holds steady on screen as a
// circle around them, and it stops just short of the hero so nothing they stand by is
// touched. Only passes seen from the camera call this; shadows stay whole.

struct VoxelCutout {
    // xyz: the camera; w: how strongly to cut (0 off, 1 full).
    camera: vec4<f32>,
    // xyz: the middle of the hero; w: the cut's radius there, in meters.
    hero: vec4<f32>,
}

@group(constants::MATERIAL_BIND_GROUP) @binding(101)
var<uniform> cutout: VoxelCutout;

// Room kept clear in front of the hero, in meters, so their own surroundings stay.
const KEEP: f32 = 0.6;
// The densest the cut gets: a few pixels stay, enough to tell something is there.
const MOST: f32 = 0.85;

// A 4x4 ordered-dither threshold for this pixel, in 0..1.
fn bayer(pixel: vec2<f32>) -> f32 {
    var pattern = array<f32, 16>(0.0, 8.0, 2.0, 10.0, 12.0, 4.0, 14.0, 6.0, 3.0, 11.0, 1.0, 9.0, 15.0, 7.0, 13.0, 5.0);
    let p = vec2<u32>(pixel) & vec2(3u);
    return (pattern[p.y * 4u + p.x] + 0.5) / 16.0;
}

// Discards this fragment if it falls in the cut.
fn cut(world_position: vec3<f32>, pixel: vec2<f32>) {
    let strength = cutout.camera.w;
    let line = cutout.hero.xyz - cutout.camera.xyz;
    let length_ = length(line);
    if strength <= 0.0 || length_ < 0.01 {
        return;
    }
    let toward = line / length_;
    let offset = world_position - cutout.camera.xyz;
    let along = dot(offset, toward);
    let gap = length_ - along;
    if along <= 0.0 || gap < KEEP {
        return;
    }
    let across = length(offset - toward * along);
    let radius = cutout.hero.w * along / length_;
    let inside = 1.0 - smoothstep(radius * 0.6, radius, across);
    let clear_of_hero = smoothstep(KEEP, KEEP + 0.6, gap);
    if bayer(pixel) < strength * inside * clear_of_hero * MOST {
        discard;
    }
}
