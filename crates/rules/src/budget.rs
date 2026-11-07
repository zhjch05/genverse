//! Budgets: how much a parcel and a player may hold, so a shared world stays cheap to
//! draw and fair to share. Per-change limits ([`crate::build::limits`],
//! [`crate::sculpt::limits`]) cap one build or edit; budgets cap what piles up.
//!
//! A build counts in full against every parcel its footprint touches, and against the
//! player who first placed it. Terrain changes count their changed voxels against
//! their author.

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use bevy_math::IVec2;
use serde::{Deserialize, Serialize};

use crate::{
    build::{BuildLimits, BuildSpec, compile},
    log::fnv1a,
};

/// What a parcel and a player may hold. Each server sets its own
/// ([`crate::settings::WorldRules`]).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetLimits {
    pub parcel_builds: u32,
    /// Visible faces of builds: what a parcel costs to draw.
    pub parcel_faces: usize,
    pub parcel_voxels: usize,
    /// Glowing voxels, which also light the scene.
    pub parcel_glow: usize,
    pub player_builds: u32,
    pub player_voxels: usize,
    /// Voxels changed by a player's terrain changes, in total.
    pub player_terrain: usize,
}

impl Default for BudgetLimits {
    fn default() -> Self {
        Self {
            parcel_builds: 12,
            parcel_faces: 40_000,
            parcel_voxels: 150_000,
            parcel_glow: 4_000,
            player_builds: 60,
            player_voxels: 1_500_000,
            player_terrain: 2_000_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub builds: u32,
    pub voxels: usize,
    pub faces: usize,
    pub glow: usize,
    pub terrain: usize,
}

impl Usage {
    fn add(&mut self, other: Usage) {
        self.builds += other.builds;
        self.voxels += other.voxels;
        self.faces += other.faces;
        self.glow += other.glow;
        self.terrain += other.terrain;
    }

    fn sub(&mut self, other: Usage) {
        self.builds = self.builds.saturating_sub(other.builds);
        self.voxels = self.voxels.saturating_sub(other.voxels);
        self.faces = self.faces.saturating_sub(other.faces);
        self.glow = self.glow.saturating_sub(other.glow);
        self.terrain = self.terrain.saturating_sub(other.terrain);
    }
}

/// What one build costs. Results are remembered by spec, since replay asks often.
pub fn build_usage(spec: &BuildSpec) -> Usage {
    static CACHE: OnceLock<Mutex<HashMap<u64, Usage>>> = OnceLock::new();
    let key = fnv1a(serde_json::to_string(spec).unwrap_or_default().as_bytes());
    let cache = CACHE.get_or_init(Default::default);
    if let Some(usage) = cache.lock().ok().and_then(|c| c.get(&key).copied()) {
        return usage;
    }
    let usage = compile(spec, &BuildLimits::ACCEPTED).map_or(Usage { builds: 1, ..Default::default() }, |b| Usage {
        builds: 1,
        voxels: b.stats.voxels,
        faces: b.stats.faces,
        glow: b.stats.glow,
        terrain: 0,
    });
    if let Ok(mut cache) = cache.lock() {
        cache.insert(key, usage);
    }
    usage
}

/// A placed build's share of the budgets.
#[derive(Clone, Debug)]
pub struct Footprint {
    pub owner: String,
    pub parcels: Vec<IVec2>,
    pub usage: Usage,
}

#[derive(Clone, Debug, Default)]
pub struct Budgets {
    pub parcels: HashMap<IVec2, Usage>,
    pub players: HashMap<String, Usage>,
}

/// "this parcel would have 45000 visible build faces (its budget is 40000)".
fn over(whose: &str, what: &str, have: usize, limit: usize) -> Option<String> {
    (have > limit).then(|| format!("{whose} would have {have} {what} (the budget is {limit})"))
}

impl Budgets {
    pub fn parcel(&self, parcel: IVec2) -> Usage {
        self.parcels.get(&parcel).copied().unwrap_or_default()
    }

    pub fn player(&self, player: &str) -> Usage {
        self.players.get(player).copied().unwrap_or_default()
    }

    /// Whether adding `new` (replacing `old`, if a build is being changed) fits. The
    /// reason names the budget it breaks.
    pub fn check(&self, new: &Footprint, old: Option<&Footprint>, limits: &BudgetLimits) -> Result<(), String> {
        let BudgetLimits {
            parcel_builds,
            parcel_faces,
            parcel_voxels,
            parcel_glow,
            player_builds,
            player_voxels,
            player_terrain,
        } = *limits;
        for parcel in &new.parcels {
            let mut u = self.parcel(*parcel);
            if let Some(old) = old.filter(|o| o.parcels.contains(parcel)) {
                u.sub(old.usage);
            }
            u.add(new.usage);
            let problem = over("this parcel", "builds", u.builds as usize, parcel_builds as usize)
                .or_else(|| over("this parcel", "visible build faces", u.faces, parcel_faces))
                .or_else(|| over("this parcel", "build voxels", u.voxels, parcel_voxels))
                .or_else(|| over("this parcel", "glowing voxels", u.glow, parcel_glow));
            if let Some(problem) = problem {
                return Err(problem);
            }
        }
        let mut u = self.player(&new.owner);
        if let Some(old) = old.filter(|o| o.owner == new.owner) {
            u.sub(old.usage);
        }
        u.add(new.usage);
        over("you", "builds", u.builds as usize, player_builds as usize)
            .or_else(|| over("you", "build voxels", u.voxels, player_voxels))
            .or_else(|| over("you", "voxels changed by terrain changes", u.terrain, player_terrain))
            .map_or(Ok(()), Err)
    }

    pub fn add(&mut self, f: &Footprint) {
        for parcel in &f.parcels {
            self.parcels.entry(*parcel).or_default().add(f.usage);
        }
        self.players.entry(f.owner.clone()).or_default().add(f.usage);
    }

    pub fn remove(&mut self, f: &Footprint) {
        for parcel in &f.parcels {
            self.parcels.entry(*parcel).or_default().sub(f.usage);
        }
        self.players.entry(f.owner.clone()).or_default().sub(f.usage);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn footprint(owner: &str, parcels: &[IVec2], faces: usize) -> Footprint {
        Footprint { owner: owner.into(), parcels: parcels.to_vec(), usage: Usage { builds: 1, faces, ..Default::default() } }
    }

    #[test]
    fn budgets_refuse_and_free_up() {
        let mut budgets = Budgets::default();
        let here = [IVec2::new(3, 3)];
        let big = footprint("a", &here, 30_000);
        budgets.add(&big);
        // Another 30k faces on the same parcel is too much...
        let err = budgets.check(&footprint("b", &here, 30_000), None, &BudgetLimits::default()).unwrap_err();
        assert!(err.contains("visible build faces"), "{err}");
        // ...but fine next door, or as a replacement of the big one.
        assert!(budgets.check(&footprint("b", &[IVec2::new(4, 3)], 30_000), None, &BudgetLimits::default()).is_ok());
        assert!(budgets.check(&footprint("a", &here, 35_000), Some(&big), &BudgetLimits::default()).is_ok());
        budgets.remove(&big);
        assert!(budgets.check(&footprint("b", &here, 30_000), None, &BudgetLimits::default()).is_ok());

        // Build count per parcel.
        for _ in 0..BudgetLimits::default().parcel_builds {
            budgets.add(&footprint("c", &here, 10));
        }
        let err = budgets.check(&footprint("c", &here, 10), None, &BudgetLimits::default()).unwrap_err();
        assert_eq!(err, "this parcel would have 13 builds (the budget is 12)");
    }
}
