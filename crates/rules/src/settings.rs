//! A world's rules, set by whoever runs it: how land works, the protected spawn, the
//! limits on one build or terrain change, budgets, rate limits, and how it looks.
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

/// How the world is drawn, for everyone in it. Changes nothing about the voxels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Look {
    /// How far voxels' outside edges are rounded, as a share of a voxel: 0 is sharp
    /// cubes, up to [`Look::MAX_BEVEL`].
    pub bevel: f32,
}

impl Look {
    pub const MAX_BEVEL: f32 = 0.3;

    fn is_default(&self) -> bool {
        *self == Self::default()
    }
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
    /// Left out while it's the default, so rules signed before it existed still verify.
    #[serde(skip_serializing_if = "Look::is_default")]
    pub look: Look,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_look_leaves_signed_rules_unchanged() {
        // Rules signed before `look` existed must serialize to the same bytes.
        let json = serde_json::to_string(&WorldRules::default()).unwrap();
        assert!(!json.contains("look"));
        let rules = WorldRules { look: Look { bevel: 0.12 }, ..Default::default() };
        let back: WorldRules = serde_json::from_str(&serde_json::to_string(&rules).unwrap()).unwrap();
        assert_eq!(back.look.bevel, 0.12);
    }
}
