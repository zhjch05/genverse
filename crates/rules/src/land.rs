//! Who may change which land: parcels, the protected spawn, and admins.
//!
//! Land is split into parcels, one chunk's footprint (8 × 8 m). A parcel is unclaimed,
//! or owned by a player who may add builders. A change must be allowed on every parcel
//! it touches; unclaimed parcels it touches are claimed for its author. The spawn area
//! can't be claimed or changed except by admins (the world's creator).
//!
//! Land isn't saved: it's derived from the change log, so replaying the log (or
//! rolling part of it back) always agrees with who owns what.

use std::collections::{BTreeSet, HashMap};

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

#[derive(Clone, Debug, PartialEq)]
pub struct Parcel {
    pub owner: String,
    pub owner_name: String,
    /// Other players allowed to change it.
    pub builders: Vec<String>,
}

/// What a player may do on one parcel.
#[derive(Clone, Debug, PartialEq)]
pub enum Access {
    Yours,
    /// Someone else's, and you're one of its builders.
    Shared,
    Free,
    Spawn,
    Taken { owner_name: String },
}

#[derive(Clone, Debug, Default)]
pub struct Land {
    pub parcels: HashMap<IVec2, Parcel>,
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

/// Parcels under a build standing at `position` (bottom-center), turned by `yaw`, with
/// `size` in build voxels.
pub fn build_parcels(position: Vec3, yaw: f32, size: &[i64]) -> Vec<IVec2> {
    let half = Vec2::new(
        size.first().copied().unwrap_or(1) as f32,
        size.get(2).copied().unwrap_or(1) as f32,
    ) * build::VOXEL_SIZE
        / 2.0;
    let (sin, cos) = yaw.sin_cos();
    // Axis-aligned extent of the turned footprint, shrunk a hair so a build sitting
    // exactly on a parcel edge doesn't count the neighbor.
    let extent = Vec2::new(cos.abs() * half.x + sin.abs() * half.y, sin.abs() * half.x + cos.abs() * half.y) - 0.01;
    let center = Vec2::new(position.x, position.z);
    let column = |p: Vec2| (p / VOXEL_SIZE).floor().as_ivec2();
    parcels_between(column(center - extent), column(center + extent))
}

impl Land {
    pub fn new(admins: Vec<String>, spawn: Spawn) -> Self {
        Self { parcels: HashMap::new(), admins, spawn }
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

    pub fn access(&self, player: &str, parcel: IVec2) -> Access {
        match self.parcels.get(&parcel) {
            Some(p) if p.owner == player => Access::Yours,
            Some(p) if p.builders.iter().any(|b| b == player) => Access::Shared,
            Some(p) => Access::Taken { owner_name: p.owner_name.clone() },
            None if self.in_spawn(parcel) => Access::Spawn,
            None => Access::Free,
        }
    }

    pub fn can_edit(&self, player: &str, parcel: IVec2) -> bool {
        self.is_admin(player) || matches!(self.access(player, parcel), Access::Yours | Access::Shared | Access::Free)
    }

    /// Checks a change touching `parcels`. Returns the unclaimed ones it would claim,
    /// or why it isn't allowed (phrased for the player and the agent).
    pub fn authorize(&self, player: &str, parcels: &[IVec2]) -> Result<Vec<IVec2>, String> {
        let mut claims = BTreeSet::new();
        for &parcel in parcels {
            match self.access(player, parcel) {
                Access::Free => {
                    claims.insert((parcel.x, parcel.y));
                }
                Access::Spawn if !self.is_admin(player) => {
                    return Err("the spawn area is protected; build at least a few steps away from it".into());
                }
                Access::Taken { owner_name } if !self.is_admin(player) => {
                    return Err(format!("that land belongs to {owner_name}"));
                }
                _ => {}
            }
        }
        Ok(claims.into_iter().map(|(x, z)| IVec2::new(x, z)).collect())
    }

    /// Checks a terrain change in `area` and keeps only the edits on land the player
    /// may change (trees reaching past the area into a neighbor's parcel are cut off).
    /// Returns the parcels it claims and the edits that remain.
    pub fn authorize_terrain(
        &self,
        player: &str,
        area: Area,
        edits: Vec<(IVec3, crate::voxel::VoxelId)>,
    ) -> Result<(Vec<IVec2>, Vec<(IVec3, crate::voxel::VoxelId)>), String> {
        let claims = self.authorize(player, &area_parcels(area))?;
        let editable = |parcel: IVec2| claims.contains(&parcel) || self.can_edit(player, parcel);
        let edits = edits.into_iter().filter(|(p, _)| editable(parcel_of(IVec2::new(p.x, p.z)))).collect();
        Ok((claims, edits))
    }

    /// Claims the free parcels among `parcels` (spawn is never claimed).
    pub fn claim(&mut self, player: &str, player_name: &str, parcels: &[IVec2]) {
        for &parcel in parcels {
            if !self.parcels.contains_key(&parcel) && !self.in_spawn(parcel) {
                let owner = Parcel { owner: player.into(), owner_name: player_name.into(), builders: Vec::new() };
                self.parcels.insert(parcel, owner);
            }
        }
    }

    /// Gives up `player`'s parcels among `parcels`.
    pub fn release(&mut self, player: &str, parcels: &[IVec2]) {
        for parcel in parcels {
            if self.parcels.get(parcel).is_some_and(|p| p.owner == player) {
                self.parcels.remove(parcel);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn land() -> Land {
        let mut land = Land::new(vec!["admin".into()], Spawn::default());
        land.claim("ada", "Ada", &[IVec2::new(10, 0)]);
        land
    }

    #[test]
    fn permission_matrix() {
        let mut land = land();
        land.parcels.get_mut(&IVec2::new(10, 0)).unwrap().builders.push("bo".into());
        let (ada, free, spawn) = (IVec2::new(10, 0), IVec2::new(12, 0), IVec2::ZERO);
        assert!(land.in_spawn(spawn) && !land.in_spawn(free));

        // Owner, builder, stranger, admin on Ada's parcel.
        assert_eq!(land.authorize("ada", &[ada]), Ok(vec![]));
        assert_eq!(land.authorize("bo", &[ada]), Ok(vec![]));
        assert!(land.authorize("cy", &[ada]).unwrap_err().contains("Ada"));
        assert!(land.authorize("admin", &[ada]).is_ok());

        // Wilderness gets claimed; spawn is admins only.
        assert_eq!(land.authorize("cy", &[free]), Ok(vec![free]));
        assert!(land.authorize("cy", &[spawn]).unwrap_err().contains("spawn"));
        assert_eq!(land.authorize("admin", &[spawn]), Ok(vec![]));
        land.claim("admin", "Admin", &[spawn]);
        assert!(!land.parcels.contains_key(&spawn));

        land.release("cy", &[ada]);
        assert!(land.parcels.contains_key(&ada));
        land.release("ada", &[ada]);
        assert!(!land.parcels.contains_key(&ada));
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
    fn terrain_edits_stop_at_a_neighbors_parcel() {
        let land = land();
        // An area in free parcel 11 whose edits spill into Ada's parcel 10 and free parcel 12.
        let area = Area { min: [11 * PARCEL, 0], size: [PARCEL, PARCEL] };
        let edits = vec![
            (IVec3::new(10 * PARCEL + 31, 40, 4), 1),
            (IVec3::new(11 * PARCEL + 5, 40, 4), 1),
            (IVec3::new(12 * PARCEL, 40, 4), 1),
        ];
        let (claims, kept) = land.authorize_terrain("cy", area, edits).unwrap();
        assert_eq!(claims, vec![IVec2::new(11, 0)]);
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|(p, _)| p.x >= 11 * PARCEL));
    }
}
