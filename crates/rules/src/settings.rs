//! A world's rules, set by whoever runs it: how land works, the protected spawn, the
//! limits on one build or terrain change, budgets, and rate limits.
//!
//! The rules travel in the change log (its header, then admins' `SetRules` changes),
//! so replay always applies the rules that were in force when each change was made.

use serde::{Deserialize, Serialize};

use crate::{budget::BudgetLimits, build::BuildLimits, land::Spawn, sculpt::TerrainLimits};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LandMode {
    /// Anyone may change any land outside the spawn.
    #[default]
    Open,
    /// Land is claimed parcel by parcel, and only owners (and their builders) change it.
    Parcels,
}

/// How fast one player may change the world. `None` is unlimited. Admins are exempt.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimits {
    /// Builds placed, moved, revised or removed, terrain changes, and claims.
    pub changes_per_minute: Option<u32>,
    /// Build voxels placed plus terrain voxels changed.
    pub voxels_per_ten_minutes: Option<usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorldRules {
    pub land: LandMode,
    pub spawn: Spawn,
    pub build: BuildLimits,
    pub terrain: TerrainLimits,
    pub budgets: BudgetLimits,
    pub rates: RateLimits,
}
