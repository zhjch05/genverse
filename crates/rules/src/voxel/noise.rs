//! Small deterministic hash and value noise, so models rasterize identically everywhere.

use bevy_math::{IVec3, Vec3};

fn hash(x: i32, y: i32, z: i32, seed: u32) -> u32 {
    let mut h = (x as u32).wrapping_mul(0x8da6_b343)
        ^ (y as u32).wrapping_mul(0xd816_3841)
        ^ (z as u32).wrapping_mul(0xcb1a_b31f)
        ^ seed.wrapping_mul(0x9e37_79b9);
    h ^= h >> 13;
    h = h.wrapping_mul(0x5bd1_e995);
    h ^= h >> 15;
    h
}

/// Uniform value in `[0, 1]` for an integer cell.
pub fn hash01(p: IVec3, seed: u32) -> f32 {
    hash(p.x, p.y, p.z, seed) as f32 / u32::MAX as f32
}

fn smooth(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

/// Trilinear value noise in `[0, 1]`.
pub fn value(p: Vec3, seed: u32) -> f32 {
    let i = p.floor().as_ivec3();
    let f = p - p.floor();
    let (u, v, w) = (smooth(f.x), smooth(f.y), smooth(f.z));
    let c = |dx, dy, dz| hash01(i + IVec3::new(dx, dy, dz), seed);
    let x00 = c(0, 0, 0) + (c(1, 0, 0) - c(0, 0, 0)) * u;
    let x10 = c(0, 1, 0) + (c(1, 1, 0) - c(0, 1, 0)) * u;
    let x01 = c(0, 0, 1) + (c(1, 0, 1) - c(0, 0, 1)) * u;
    let x11 = c(0, 1, 1) + (c(1, 1, 1) - c(0, 1, 1)) * u;
    let y0 = x00 + (x10 - x00) * v;
    let y1 = x01 + (x11 - x01) * v;
    y0 + (y1 - y0) * w
}

/// Fractal value noise in `[0, 1]`.
pub fn fbm(p: Vec3, seed: u32) -> f32 {
    let (mut sum, mut amp, mut freq, mut norm) = (0.0, 0.5, 1.0, 0.0);
    for octave in 0..4 {
        sum += value(p * freq, seed.wrapping_add(octave)) * amp;
        norm += amp;
        amp *= 0.5;
        freq *= 2.03;
    }
    sum / norm
}
