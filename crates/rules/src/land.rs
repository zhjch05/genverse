//! Who may change which land: anyone who can edit the world may change any of it,
//! except the protected spawn, which only admins change.
//!
//! Land used to be claimed parcel by parcel; claims are retired, like editing a shared
//! document, where anyone with edit access changes anything. Who is working where right
//! now is shown and held off live, outside the log (the server's work locks), and what
//! a joined player may change will be the admins' to set. Parcels stay as a grid: a
//! game can run on some of them, and budgets are kept by them.
//!
//! Land isn't saved: it's derived from the change log.

use bevy_math::{IVec2, IVec3, Vec2, Vec3};
use serde::{Deserialize, Serialize};

use crate::{build, sculpt::Area, terrain::VOXEL_SIZE};

/// Parcel edge length in terrain voxels (one chunk).
pub const PARCEL: i32 = 32;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Spawn {
    /// Terrain column at the center.
    pub center: [i32; 2],
    pub radius_m: f32,
}

impl Default for Spawn {
    fn default() -> Self {
        Self { center: [0, 0], radius_m: 24.0 }
    }
}

/// What a player may do on one parcel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Free,
    /// The protected spawn: admins only.
    Spawn,
}

#[derive(Clone, Debug, Default)]
pub struct Land {
    pub admins: Vec<String>,
    pub spawn: Spawn,
}

pub fn parcel_of(column: IVec2) -> IVec2 {
    column.div_euclid(IVec2::splat(PARCEL))
}

/// Every parcel overlapping the columns `min..=max`.
pub fn parcels_between(min: IVec2, max: IVec2) -> Vec<IVec2> {
    let (lo, hi) = (parcel_of(min.min(max)), parcel_of(min.max(max)));
    (lo.y..=hi.y).flat_map(|z| (lo.x..=hi.x).map(move |x| IVec2::new(x, z))).collect()
}

pub fn area_parcels(area: Area) -> Vec<IVec2> {
    parcels_between(area.min(), area.min() + area.size() - 1)
}

/// The columns under a footprint centered at `center` (meters, x and z), turned by
/// `yaw`, `size` meters wide and deep.
pub fn footprint(center: Vec2, yaw: f32, size: Vec2) -> Area {
    let half = size / 2.0;
    let (sin, cos) = yaw.sin_cos();
    // Axis-aligned extent of the turned footprint, shrunk a hair so a build sitting
    // exactly on a column edge doesn't count the neighbor.
    let extent = (Vec2::new(cos.abs() * half.x + sin.abs() * half.y, sin.abs() * half.x + cos.abs() * half.y) - 0.01).max(Vec2::ZERO);
    let column = |p: Vec2| (p / VOXEL_SIZE).floor().as_ivec2();
    Area::from_corners(column(center - extent), column(center + extent))
}

/// The columns under a build standing at `position` (bottom-center), turned by `yaw`,
/// with `size` in build voxels.
pub fn build_area(position: Vec3, yaw: f32, size: &[i64]) -> Area {
    let size = Vec2::new(size.first().copied().unwrap_or(1) as f32, size.get(2).copied().unwrap_or(1) as f32) * build::VOXEL_SIZE;
    footprint(Vec2::new(position.x, position.z), yaw, size)
}

/// Parcels under a build standing at `position` (bottom-center), turned by `yaw`, with
/// `size` in build voxels.
pub fn build_parcels(position: Vec3, yaw: f32, size: &[i64]) -> Vec<IVec2> {
    area_parcels(build_area(position, yaw, size))
}

impl Land {
    pub fn new(admins: Vec<String>, spawn: Spawn) -> Self {
        Self { admins, spawn }
    }

    pub fn is_admin(&self, player: &str) -> bool {
        self.admins.iter().any(|a| a == player)
    }

    /// Whether `parcel` overlaps the protected spawn square.
    pub fn in_spawn(&self, parcel: IVec2) -> bool {
        let r = (self.spawn.radius_m / VOXEL_SIZE).ceil() as i32;
        let center = IVec2::from_array(self.spawn.center);
        let (lo, hi) = (parcel * PARCEL, parcel * PARCEL + PARCEL - 1);
        lo.cmple(center + r).all() && hi.cmpge(center - r).all()
    }

    /// The protected spawn as the columns it covers: every parcel reaching into the
    /// square of `radius_m` around the center, which together make one rectangle. What
    /// the spawn area overlay draws.
    pub fn spawn_area(&self) -> Area {
        let r = (self.spawn.radius_m / VOXEL_SIZE).ceil() as i32;
        let center = IVec2::from_array(self.spawn.center);
        let (lo, hi) = (parcel_of(center - r), parcel_of(center + r));
        Area::from_corners(lo * PARCEL, hi * PARCEL + PARCEL - 1)
    }

    pub fn access(&self, parcel: IVec2) -> Access {
        if self.in_spawn(parcel) { Access::Spawn } else { Access::Free }
    }

    /// Checks a change touching `parcels`: why it isn't allowed, phrased for the player
    /// and the agent.
    pub fn authorize(&self, player: &str, parcels: &[IVec2]) -> Result<(), String> {
        if !self.is_admin(player) && parcels.iter().any(|p| self.in_spawn(*p)) {
            return Err("the spawn area is protected; build at least a few steps away from it".into());
        }
        Ok(())
    }

    /// Checks a terrain change in `area`; returns its edits.
    pub fn authorize_terrain(
        &self,
        player: &str,
        area: Area,
        edits: Vec<(IVec3, crate::voxel::VoxelId)>,
    ) -> Result<Vec<(IVec3, crate::voxel::VoxelId)>, String> {
        self.authorize(player, &area_parcels(area))?;
        Ok(edits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anyone_changes_any_land_but_the_spawn() {
        let land = Land::new(vec!["admin".into()], Spawn::default());
        let (far, spawn) = (IVec2::new(10, 0), IVec2::ZERO);
        assert!(land.in_spawn(spawn) && !land.in_spawn(far));
        assert_eq!(land.authorize("cy", &[far]), Ok(()));
        assert!(land.authorize("cy", &[far, spawn]).unwrap_err().contains("spawn"));
        assert_eq!(land.authorize("admin", &[spawn]), Ok(()));
        assert_eq!((land.access(far), land.access(spawn)), (Access::Free, Access::Spawn));
    }

    #[test]
    fn footprints_cover_turned_builds() {
        // A 4 m wide, 1 m deep build centered on a parcel corner touches four parcels;
        // turned a quarter, the same at a parcel's center touches one.
        let corner = Vec3::new(8.0, 0.0, 8.0);
        assert_eq!(build_parcels(corner, 0.0, &[32, 8, 8]).len(), 4);
        let center = Vec3::new(12.0, 0.0, 12.0);
        assert_eq!(build_parcels(center, std::f32::consts::FRAC_PI_2, &[32, 8, 8]), vec![IVec2::new(1, 1)]);
    }

    #[test]
    fn the_spawn_area_is_exactly_the_protected_parcels() {
        for (center, radius_m) in [([0, 0], 24.0), ([37, -90], 3.0), ([5, 5], 0.0), ([-200, 64], 100.0)] {
            let land = Land { admins: Vec::new(), spawn: Spawn { center, radius_m } };
            let area = land.spawn_area();
            for z in -12..=12 {
                for x in -12..=12 {
                    let parcel = IVec2::new(x, z);
                    let inside = area.contains(parcel * PARCEL) && area.contains(parcel * PARCEL + PARCEL - 1);
                    assert_eq!(land.in_spawn(parcel), inside, "{center:?} {radius_m}: parcel {parcel}");
                }
            }
        }
    }
}
