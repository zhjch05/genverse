//! Small deterministic hash and value noise, so models rasterize identically everywhere.

use bevy_math::{IVec3, Vec2, Vec3};

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

/// [`value`] in the plane y = 0, where it needs only the 4 corners at y = 0: the same
/// result for half the hashing.
pub fn value_xz(p: Vec2, seed: u32) -> f32 {
    let i = p.floor().as_ivec2();
    let f = p - p.floor();
    let (u, w) = (smooth(f.x), smooth(f.y));
    let c = |dx, dz| hash01(IVec3::new(i.x + dx, 0, i.y + dz), seed);
    let x00 = c(0, 0) + (c(1, 0) - c(0, 0)) * u;
    let x01 = c(0, 1) + (c(1, 1) - c(0, 1)) * u;
    x00 + (x01 - x00) * w
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

/// [`fbm`] in the plane y = 0.
pub fn fbm_xz(p: Vec2, seed: u32) -> f32 {
    let (mut sum, mut amp, mut freq, mut norm) = (0.0, 0.5, 1.0, 0.0);
    for octave in 0..4 {
        sum += value_xz(p * freq, seed.wrapping_add(octave)) * amp;
        norm += amp;
        amp *= 0.5;
        freq *= 2.03;
    }
    sum / norm
}

/// [`value`] at every integer point of a box, for `p / scale`: each lattice corner is
/// hashed once rather than once per point near it, with the same arithmetic per axis,
/// so the results are identical.
pub struct ValueGrid {
    /// Per axis and point: the lattice cell (relative to the box's first) and the
    /// smoothed fraction across it.
    axes: [Vec<(usize, f32)>; 3],
    /// Lattice corners, x fastest.
    stride: [usize; 2],
    corners: Vec<f32>,
}

impl ValueGrid {
    pub fn new(min: IVec3, size: IVec3, scale: f32, seed: u32) -> Self {
        let mut first = [0; 3];
        let mut cells = [0; 3];
        let axes = [0, 1, 2].map(|a| {
            let points: Vec<(i32, f32)> = (min[a]..min[a] + size[a])
                .map(|v| {
                    let p = v as f32 / scale;
                    (p.floor() as i32, smooth(p - p.floor()))
                })
                .collect();
            first[a] = points.first().map_or(0, |p| p.0);
            cells[a] = points.last().map_or(0, |p| p.0) - first[a] + 2;
            points
                .into_iter()
                .map(|(i, f)| ((i - first[a]) as usize, f))
                .collect()
        });
        let mut corners = Vec::with_capacity((cells[0] * cells[1] * cells[2]) as usize);
        for z in 0..cells[2] {
            for y in 0..cells[1] {
                for x in 0..cells[0] {
                    corners.push(hash01(
                        IVec3::new(first[0] + x, first[1] + y, first[2] + z),
                        seed,
                    ));
                }
            }
        }
        let stride = [cells[0] as usize, (cells[0] * cells[1]) as usize];
        Self {
            axes,
            stride,
            corners,
        }
    }

    /// The noise at the point `local` from the box's corner.
    pub fn get(&self, local: IVec3) -> f32 {
        let (ix, u) = self.axes[0][local.x as usize];
        let (iy, v) = self.axes[1][local.y as usize];
        let (iz, w) = self.axes[2][local.z as usize];
        let [sy, sz] = self.stride;
        let i = ix + iy * sy + iz * sz;
        let c = |d: usize| self.corners[i + d];
        let x00 = c(0) + (c(1) - c(0)) * u;
        let x10 = c(sy) + (c(sy + 1) - c(sy)) * u;
        let x01 = c(sz) + (c(sz + 1) - c(sz)) * u;
        let x11 = c(sy + sz) + (c(sy + sz + 1) - c(sy + sz)) * u;
        let y0 = x00 + (x10 - x00) * v;
        let y1 = x01 + (x11 - x01) * v;
        y0 + (y1 - y0) * w
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_paths_match_value_noise_exactly() {
        for (x, z) in [(0, 0), (-37, 211), (1234, -987), (5, 5)] {
            let p = Vec2::new(x as f32, z as f32) / 70.0;
            assert_eq!(value_xz(p, 3), value(Vec3::new(p.x, 0.0, p.y), 3));
            assert_eq!(fbm_xz(p, 3), fbm(Vec3::new(p.x, 0.0, p.y), 3));
        }
        let min = IVec3::new(-13, 7, 40);
        for scale in [4.0, 5.0] {
            let grid = ValueGrid::new(min, IVec3::splat(12), scale, 9);
            for z in 0..12 {
                for y in 0..12 {
                    for x in 0..12 {
                        let local = IVec3::new(x, y, z);
                        assert_eq!(grid.get(local), value((min + local).as_vec3() / scale, 9));
                    }
                }
            }
        }
    }
}
