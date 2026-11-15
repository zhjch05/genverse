//! Spatial queries against placed [`VoxelModel`]s: box overlap for collision and
//! ray casts for picking. Both work in the model's own voxel space, so they respect
//! its rotation and pivot.

// Picking is only used by the desktop builder; the web build only needs overlap tests.
#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

use bevy::prelude::*;

use super::{AIR, VoxelModel};

/// World point -> model voxel coordinates (fractional).
fn to_voxels(model: &VoxelModel, inverse: &bevy::math::Affine3A, p: Vec3) -> Vec3 {
    inverse.transform_point3(p) / model.voxel_size + model.pivot
}

/// Whether `model` has a solid voxel at world point `p`; `inverse` undoes its transform.
pub fn solid_at(model: &VoxelModel, inverse: &bevy::math::Affine3A, p: Vec3) -> bool {
    let cell = to_voxels(model, inverse, p).floor().as_ivec3();
    model.grid.contains(cell) && model.grid.get(cell) != AIR
}

/// Whether any voxel of `model` overlaps the world-space box `min..max`.
pub fn overlaps_box(model: &VoxelModel, transform: &GlobalTransform, min: Vec3, max: Vec3) -> bool {
    let inverse = transform.affine().inverse();
    let mut lo = Vec3::splat(f32::INFINITY);
    let mut hi = Vec3::splat(f32::NEG_INFINITY);
    for i in 0..8 {
        let corner = Vec3::new(
            if i & 1 == 0 { min.x } else { max.x },
            if i & 2 == 0 { min.y } else { max.y },
            if i & 4 == 0 { min.z } else { max.z },
        );
        let v = to_voxels(model, &inverse, corner);
        lo = lo.min(v);
        hi = hi.max(v);
    }
    let size = model.grid.size().as_ivec3();
    let lo = (lo + 1e-4).floor().as_ivec3().max(IVec3::ZERO);
    let hi = (hi - 1e-4).floor().as_ivec3().min(size - 1);
    for z in lo.z..=hi.z {
        for y in lo.y..=hi.y {
            for x in lo.x..=hi.x {
                if model.grid.get(IVec3::new(x, y, z)) != AIR {
                    return true;
                }
            }
        }
    }
    false
}

/// A ray hit: distance along the ray (world units) and the world-space normal of the
/// face that was entered.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    pub distance: f32,
    pub normal: Vec3,
}

/// Walks the voxels a ray passes through (Amanatides & Woo), in a space where voxels
/// are unit cubes. `solid` returns `None` to stop the walk (e.g. unloaded space).
/// `t` is measured in whatever units `dir` is scaled to. Returns `t` and the local
/// normal of the entered face (zero if the ray starts inside a solid voxel).
pub fn march(
    origin: Vec3,
    dir: Vec3,
    enter: f32,
    exit: f32,
    solid: impl Fn(IVec3) -> Option<bool>,
) -> Option<(f32, IVec3)> {
    let inv = dir.recip();
    let start = origin + dir * enter;
    let mut cell = (start + dir.signum() * 1e-4).floor().as_ivec3();
    let step = dir.signum().as_ivec3();
    let delta = inv.abs();
    let boundary = cell.as_vec3() + step.max(IVec3::ZERO).as_vec3();
    let mut next = Vec3::select(
        dir.cmpeq(Vec3::ZERO),
        Vec3::INFINITY,
        (boundary - origin) * inv,
    );
    let mut t = enter;
    let mut normal = IVec3::ZERO;
    while t <= exit {
        if solid(cell)? {
            return Some((t, normal));
        }
        let axis = if next.x < next.y && next.x < next.z {
            0
        } else if next.y < next.z {
            1
        } else {
            2
        };
        t = next[axis];
        cell[axis] += step[axis];
        next[axis] += delta[axis];
        normal = IVec3::ZERO;
        normal[axis] = -step[axis];
    }
    None
}

/// The first solid voxel of `model` hit by `ray` within `max`.
pub fn raycast(
    model: &VoxelModel,
    transform: &GlobalTransform,
    ray: Ray3d,
    max: f32,
) -> Option<Hit> {
    let affine = transform.affine();
    let inverse = affine.inverse();
    let origin = to_voxels(model, &inverse, ray.origin);
    // Not normalized: `t` stays in world units along the original ray.
    let dir = inverse.transform_vector3(*ray.direction) / model.voxel_size;
    let size = model.grid.size().as_vec3();

    // Clip the ray to the grid's bounds.
    let inv = dir.recip();
    let t0 = (Vec3::ZERO - origin) * inv;
    let t1 = (size - origin) * inv;
    let near = t0.min(t1);
    let enter = near.max_element().max(0.0);
    let exit = t0.max(t1).min_element().min(max);
    if enter > exit {
        return None;
    }
    let (distance, mut normal) = march(origin, dir, enter, exit, |cell| {
        model
            .grid
            .contains(cell)
            .then(|| model.grid.get(cell) != AIR)
    })?;
    // A hit in the very first cell entered through the bounding box face.
    if normal == IVec3::ZERO && enter > 0.0 {
        let axis = near.max_position();
        normal[axis] = -step_sign(dir[axis]);
    }
    Some(Hit {
        distance,
        normal: affine
            .transform_vector3(normal.as_vec3())
            .normalize_or_zero(),
    })
}

fn step_sign(v: f32) -> i32 {
    if v > 0.0 { 1 } else { -1 }
}

#[cfg(test)]
mod tests {
    use std::{f32::consts::FRAC_PI_2, sync::Arc};

    use super::*;
    use genverse_rules::voxel::{Palette, PaletteEntry, Surface, VoxelGrid};

    /// A 4x2x2 bar of voxels, 0.5 m each, pivoting at its bottom-center.
    fn bar() -> VoxelModel {
        let mut grid = VoxelGrid::new(UVec3::new(4, 2, 2));
        for x in 0..4 {
            for y in 0..2 {
                for z in 0..2 {
                    grid.set(IVec3::new(x, y, z), 1);
                }
            }
        }
        let surface = Surface {
            name: "s".into(),
            roughness: 1.0,
            metallic: 0.0,
            reflectance: 0.5,
            emissive: LinearRgba::BLACK,
            alpha: 1.0,
        };
        let entry = |name: &str| PaletteEntry {
            name: name.into(),
            color: LinearRgba::WHITE,
            surface: 0,
            variation: 0.0,
        };
        VoxelModel {
            grid,
            palette: Arc::new(Palette {
                surfaces: vec![surface],
                entries: vec![entry("air"), entry("solid")],
            }),
            voxel_size: 0.5,
            pivot: Vec3::new(2.0, 0.0, 1.0),
            lights: Vec::new(),
        }
    }

    fn at(x: f32, z: f32, yaw: f32) -> GlobalTransform {
        GlobalTransform::from(
            Transform::from_xyz(x, 0.0, z).with_rotation(Quat::from_rotation_y(yaw)),
        )
    }

    #[test]
    fn solid_at_respects_rotation() {
        // Turned 90 degrees the bar is long along z: x -0.5..0.5, z -1..1.
        let turned = at(10.0, 0.0, FRAC_PI_2);
        let inverse = turned.affine().inverse();
        assert!(solid_at(&bar(), &inverse, Vec3::new(10.0, 0.5, 0.8)));
        assert!(!solid_at(&bar(), &inverse, Vec3::new(10.8, 0.5, 0.0)));
        assert!(!solid_at(&bar(), &inverse, Vec3::new(10.0, 1.2, 0.0)));
    }

    #[test]
    fn ray_hits_front_face() {
        // The bar spans x -1..1, y 0..1, z -0.5..0.5 around (10, 0, 0).
        let ray = Ray3d::new(Vec3::new(10.0, 0.5, 5.0), Dir3::NEG_Z);
        let hit = raycast(&bar(), &at(10.0, 0.0, 0.0), ray, 100.0).unwrap();
        assert!((hit.distance - 4.5).abs() < 1e-3, "hit at {}", hit.distance);
        assert!(
            hit.normal.abs_diff_eq(Vec3::Z, 1e-4),
            "normal {}",
            hit.normal
        );
    }

    #[test]
    fn ray_respects_rotation() {
        // Turned 90 degrees the bar is long along z: x -0.5..0.5, z -1..1.
        let model = bar();
        let turned = at(10.0, 0.0, FRAC_PI_2);
        let past_short_side = Ray3d::new(Vec3::new(10.75, 0.5, 5.0), Dir3::NEG_Z);
        assert!(raycast(&model, &turned, past_short_side, 100.0).is_none());
        let down_long_side = Ray3d::new(Vec3::new(10.0, 0.5, 5.0), Dir3::NEG_Z);
        let hit = raycast(&model, &turned, down_long_side, 100.0).unwrap();
        assert!((hit.distance - 4.0).abs() < 1e-3, "hit at {}", hit.distance);
        assert!(
            hit.normal.abs_diff_eq(Vec3::Z, 1e-4),
            "normal {}",
            hit.normal
        );
    }

    #[test]
    fn ray_misses_beyond_reach() {
        let ray = Ray3d::new(Vec3::new(10.0, 0.5, 5.0), Dir3::NEG_Z);
        assert!(raycast(&bar(), &at(10.0, 0.0, 0.0), ray, 3.0).is_none());
    }

    #[test]
    fn ray_from_above_hits_top() {
        let ray = Ray3d::new(Vec3::new(0.3, 5.0, 0.2), Dir3::NEG_Y);
        let hit = raycast(&bar(), &at(0.0, 0.0, 0.0), ray, 100.0).unwrap();
        assert!((hit.distance - 4.0).abs() < 1e-3, "hit at {}", hit.distance);
        assert!(
            hit.normal.abs_diff_eq(Vec3::Y, 1e-4),
            "normal {}",
            hit.normal
        );
    }

    #[test]
    fn box_overlap() {
        let model = bar();
        let t = at(0.0, 0.0, 0.0);
        assert!(overlaps_box(
            &model,
            &t,
            Vec3::new(0.9, 0.0, 0.0),
            Vec3::new(1.5, 1.0, 0.2)
        ));
        assert!(!overlaps_box(
            &model,
            &t,
            Vec3::new(1.01, 0.0, 0.0),
            Vec3::new(1.5, 1.0, 0.2)
        ));
        // Standing on top touches nothing.
        assert!(!overlaps_box(
            &model,
            &t,
            Vec3::new(-0.2, 1.0, -0.2),
            Vec3::new(0.2, 2.0, 0.2)
        ));
    }
}
