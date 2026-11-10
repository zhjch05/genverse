//! A world's rules, set by whoever runs it: how land works, the protected spawn, the
//! limits on one build or terrain change, budgets, rate limits, and how it looks.
//!
//! The rules travel in the change log (its header, then admins' `SetRules` changes),
//! so replay always applies the rules that were in force when each change was made.

use serde::{Deserialize, Serialize};

use crate::{budget::BudgetLimits, build::BuildLimits, content::Content, land::Spawn, logic::LogicRules, sculpt::TerrainLimits};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LandMode {
    /// Anyone may change any land outside the spawn.
    #[default]
    Open,
    /// Retired: land was claimed parcel by parcel. Read from old rules; changes nothing.
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

/// How heroes move in this world, before what a game does to one player (`body`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HeroRules {
    /// Meters a second.
    pub walk_speed: f32,
    pub run_speed: f32,
    /// How fast a jump leaves the ground, meters a second: with the usual gravity, 7.5
    /// goes a little over a meter high.
    pub jump_speed: f32,
    /// Meters a second, each second.
    pub gravity: f32,
}

impl Default for HeroRules {
    fn default() -> Self {
        Self { walk_speed: 4.0, run_speed: 7.0, jump_speed: 7.5, gravity: 24.0 }
    }
}

impl HeroRules {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The rules kept within what a hero can be moved by, whatever a log says.
    pub fn bounded(self) -> Self {
        let within = |v: f32, lo: f32, hi: f32, usual: f32| if v.is_finite() { v.clamp(lo, hi) } else { usual };
        Self {
            walk_speed: within(self.walk_speed, 0.5, 20.0, 4.0),
            run_speed: within(self.run_speed, 0.5, 40.0, 7.0),
            jump_speed: within(self.jump_speed, 0.0, 30.0, 7.5),
            gravity: within(self.gravity, 1.0, 100.0, 24.0),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorldRules {
    /// Retired ([`LandMode::Parcels`]): kept so rules signed with it still verify.
    pub land: LandMode,
    pub spawn: Spawn,
    pub build: BuildLimits,
    pub terrain: TerrainLimits,
    pub budgets: BudgetLimits,
    pub rates: RateLimits,
    /// Left out while it's the default, so rules signed before it existed still verify.
    #[serde(skip_serializing_if = "Look::is_default")]
    pub look: Look,
    /// Who may install game packages, the world clock, and packages' limits. Left out
    /// while it's the default, like `look`.
    #[serde(skip_serializing_if = "LogicRules::is_default")]
    pub logic: LogicRules,
    /// How heroes move here. Left out while it's the default, like `look`.
    #[serde(skip_serializing_if = "HeroRules::is_default")]
    pub hero: HeroRules,
    /// The world's blocks: colors over the ground's, and blocks of its own. Left out
    /// while there are none, like `look`.
    #[serde(skip_serializing_if = "Content::is_default")]
    pub content: Content,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_look_leaves_signed_rules_unchanged() {
        // Rules signed before `look` existed must serialize to the same bytes.
        let json = serde_json::to_string(&WorldRules::default()).unwrap();
        assert!(!json.contains("look") && !json.contains("logic") && !json.contains("hero") && !json.contains("content"));
        // Hero rules travel the same way, and are bounded where they're used.
        let rules = WorldRules { hero: HeroRules { run_speed: 14.0, gravity: 1e9, ..Default::default() }, ..Default::default() };
        let back: WorldRules = serde_json::from_str(&serde_json::to_string(&rules).unwrap()).unwrap();
        assert_eq!((back.hero.run_speed, back.hero.bounded().gravity), (14.0, 100.0));
        assert_eq!(HeroRules { jump_speed: f32::NAN, ..Default::default() }.bounded().jump_speed, 7.5);
        let rules = WorldRules { look: Look { bevel: 0.12 }, ..Default::default() };
        let back: WorldRules = serde_json::from_str(&serde_json::to_string(&rules).unwrap()).unwrap();
        assert_eq!(back.look.bevel, 0.12);
    }
}
