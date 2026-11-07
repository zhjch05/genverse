//! The world's change log in the game ([`cubelark_rules::log`]): loading it, recording
//! changes with who made them, and saving.
//!
//! On desktop the log is `saves/world.json`. Replaying it is the truth;
//! `saves/world_edits.json` is a cache of the replayed voxels, used when its hash
//! matches the log. Saves from before the log are migrated once (the old files are
//! kept as `.bak`). The local player is `saves/profile.json`; the world's first player
//! is its admin. `CUBELARK_AS=Name` plays as a different (non-admin) player for this
//! run, to try the land rules from someone else's side.
//!
//! The web build has no saves and no builder yet, so nothing records there.
#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

use std::collections::{BTreeMap, HashMap};

use bevy::prelude::*;
use cubelark_rules::{
    budget::{Budgets, Footprint, Usage, limits as budget},
    land::Land,
    log::{Action, BuildState, Provenance, WorldLog, replay, replay_land},
};
use serde::{Deserialize, Serialize};

use crate::voxel::VoxelId;

#[cfg(not(target_arch = "wasm32"))]
const LOG_PATH: &str = "saves/world.json";
#[cfg(not(target_arch = "wasm32"))]
const CACHE_PATH: &str = "saves/world_edits.json";
#[cfg(not(target_arch = "wasm32"))]
const PROFILE_PATH: &str = "saves/profile.json";

/// The local player.
#[derive(Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
}

#[derive(Resource)]
pub struct Ledger {
    pub log: WorldLog,
    pub profile: Profile,
    /// Placed builds as of loading, for the builder to spawn.
    pub builds: BTreeMap<u64, BuildState>,
    /// Who owns what, as of the latest change.
    pub land: Land,
    /// What's used of each parcel's and player's budget.
    pub budgets: Budgets,
    /// Each placed build's share of the budgets.
    pub footprints: HashMap<u64, Footprint>,
    /// Bumped on every recorded change, so the voxel cache knows to follow.
    pub version: u64,
}

impl Ledger {
    /// Appends a change made by the local player (through `agent`, if one did it) and
    /// saves the log. Returns the change's id.
    pub fn record(&mut self, agent: Option<&str>, prompt: Option<&str>, action: Action) -> u64 {
        let by = Provenance::new(&self.profile.id, &self.profile.name, agent, prompt, now());
        let id = self.log.push(by, action);
        self.version += 1;
        let state = replay_land(&self.log);
        self.land = state.land;
        self.budgets = state.budgets;
        self.footprints = state.footprints;
        self.save();
        id
    }

    /// Whether the local player may change `parcels`; returns the free ones the change
    /// would claim, or why not.
    pub fn check(&self, parcels: &[IVec2]) -> Result<Vec<IVec2>, String> {
        self.land.authorize(&self.profile.id, parcels)
    }

    /// Undoes change `id` by appending a revert of it.
    pub fn revert(&mut self, change: u64) {
        self.record(None, None, Action::Revert { change });
    }

    /// The fullest of `parcels`, as "parcel budget: 2/12 builds · 18k/40k faces · …".
    pub fn parcel_line(&self, parcels: &[IVec2]) -> String {
        let fullest = parcels
            .iter()
            .map(|p| self.budgets.parcel(*p))
            .max_by_key(|u| u.faces * 1000 / budget::PARCEL_FACES.max(1) + u.builds as usize * 1000 / budget::PARCEL_BUILDS as usize)
            .unwrap_or_default();
        format!(
            "Parcel budget: {}/{} builds · {}/{} faces · {}/{} glow",
            fullest.builds,
            budget::PARCEL_BUILDS,
            short(fullest.faces),
            short(budget::PARCEL_FACES),
            short(fullest.glow),
            short(budget::PARCEL_GLOW)
        )
    }

    /// The local player's totals.
    pub fn player_line(&self) -> String {
        let u: Usage = self.budgets.player(&self.profile.id);
        format!(
            "Your budget: {}/{} builds · {}/{} build voxels · {}/{} terrain voxels",
            u.builds,
            budget::PLAYER_BUILDS,
            short(u.voxels),
            short(budget::PLAYER_VOXELS),
            short(u.terrain),
            short(budget::PLAYER_TERRAIN)
        )
    }

    /// Renames the local player; past changes keep the name they were made under.
    pub fn rename(&mut self, name: &str) {
        let name = name.trim();
        if name.is_empty() || name == self.profile.name {
            return;
        }
        self.profile.name = name.chars().take(32).collect();
        // A guest (CUBELARK_AS) is only for this run.
        #[cfg(not(target_arch = "wasm32"))]
        if !self.profile.id.starts_with("guest:") {
            write_json(PROFILE_PATH, &self.profile);
        }
    }

    /// Who first placed build `build`, and who changed it last (if anyone did since).
    pub fn credits(&self, build: u64) -> Option<(&Provenance, Option<&Provenance>)> {
        let mut changes = self.log.effective().filter(|c| match &c.action {
            Action::PlaceBuild(b) | Action::UpdateBuild(b) => b.build == build,
            _ => false,
        });
        let first = changes.next()?;
        Some((&first.by, changes.last().map(|c| &c.by)))
    }

    /// A short credit line: "Ada with Claude Code, 2 h ago" ("you" for the local player).
    pub fn describe(&self, by: &Provenance) -> String {
        let who = if by.player == self.profile.id { "you".to_string() } else { by.player_name.clone() };
        let how = by.agent.as_deref().map(|a| format!(" with {a}")).unwrap_or_default();
        format!("{who}{how}, {}", ago(now().saturating_sub(by.at)))
    }

    fn save(&self) {
        #[cfg(not(target_arch = "wasm32"))]
        write_json(LOG_PATH, &self.log);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// `SystemTime` isn't available on the web; there's no saved log there anyway.
#[cfg(target_arch = "wasm32")]
fn now() -> u64 {
    0
}

/// 18400 → "18k", 1500000 → "1.5M".
fn short(n: usize) -> String {
    match n {
        0..1000 => n.to_string(),
        1000..1_000_000 => format!("{}k", n / 1000),
        _ => format!("{:.1}M", n as f32 / 1e6),
    }
}

fn ago(seconds: u64) -> String {
    match seconds {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86_400 => format!("{} h ago", seconds / 3600),
        _ => format!("{} days ago", seconds / 86_400),
    }
}

/// The voxel cache: the replayed edits and the log they came from.
#[derive(Serialize, Deserialize)]
struct SavedEdits {
    seed: u32,
    /// Hash of the log these edits replay; missing in saves from before the log.
    #[serde(default)]
    log_hash: Option<u64>,
    /// `[x, y, z, block]` in world voxel coordinates.
    edits: Vec<[i32; 4]>,
}

/// Writes the voxel cache for the current log.
pub fn write_cache(log: &WorldLog, edits: Vec<[i32; 4]>) {
    let saved = SavedEdits { seed: log.seed, log_hash: Some(log.hash()), edits };
    #[cfg(not(target_arch = "wasm32"))]
    write_json(CACHE_PATH, &saved);
    #[cfg(target_arch = "wasm32")]
    let _ = saved;
}

#[cfg(not(target_arch = "wasm32"))]
fn write_json(path: &str, value: &impl Serialize) {
    let result = std::fs::create_dir_all("saves")
        .and_then(|_| std::fs::write(path, serde_json::to_string(value).unwrap_or_default()));
    if let Err(e) = result {
        error!("couldn't save {path}: {e}");
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn read_json<T: serde::de::DeserializeOwned>(path: &str) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).map_err(|e| error!("couldn't read {path}: {e}")).ok()
}

/// Loads the log (or migrates old saves into one) and returns it with the voxel edits
/// it describes.
pub fn load(seed: u32) -> (Ledger, HashMap<IVec3, VoxelId>) {
    let profile = load_profile();
    let (mut log, cached) = load_log(seed);
    // The world's first player runs it.
    if log.admins.is_empty() {
        log.admins.push(profile.id.clone());
        #[cfg(not(target_arch = "wasm32"))]
        write_json(LOG_PATH, &log);
    }
    // Builds and land always come from the log (cheap); voxels from the cache when it
    // matches, otherwise from a full replay.
    let (replayed, edits) = match cached {
        Some(edits) => (replay_land(&log), edits),
        None => {
            let mut full = replay(&log);
            let edits = std::mem::take(&mut full.edits);
            (full, edits)
        }
    };
    for problem in &replayed.problems {
        warn!("saved change no longer applies: {problem}");
    }
    let profile = guest().unwrap_or(profile);
    let ledger = Ledger {
        log,
        profile,
        builds: replayed.builds,
        land: replayed.land,
        budgets: replayed.budgets,
        footprints: replayed.footprints,
        version: 1,
    };
    (ledger, edits)
}

/// `CUBELARK_AS=Name`: a separate player for this run (not saved, never an admin).
fn guest() -> Option<Profile> {
    #[cfg(target_arch = "wasm32")]
    return None;
    #[cfg(not(target_arch = "wasm32"))]
    std::env::var("CUBELARK_AS").ok().filter(|n| !n.trim().is_empty()).map(|name| {
        info!("playing as {name}");
        Profile { id: format!("guest:{}", name.trim().to_lowercase()), name: name.trim().into() }
    })
}

#[cfg(target_arch = "wasm32")]
fn load_profile() -> Profile {
    Profile { id: "web".into(), name: "Player".into() }
}

#[cfg(not(target_arch = "wasm32"))]
fn load_profile() -> Profile {
    if let Some(profile) = read_json(PROFILE_PATH) {
        return profile;
    }
    use std::hash::{BuildHasher, Hasher};
    let id = (0..2)
        .map(|_| format!("{:016x}", std::collections::hash_map::RandomState::new().build_hasher().finish()))
        .collect();
    let profile = Profile { id, name: "Player".into() };
    write_json(PROFILE_PATH, &profile);
    profile
}

/// The log, and the cached voxels if they match it.
#[cfg(target_arch = "wasm32")]
fn load_log(seed: u32) -> (WorldLog, Option<HashMap<IVec3, VoxelId>>) {
    (WorldLog::new(seed), None)
}

#[cfg(not(target_arch = "wasm32"))]
fn load_log(seed: u32) -> (WorldLog, Option<HashMap<IVec3, VoxelId>>) {
    let cache: Option<SavedEdits> = read_json(CACHE_PATH);
    let to_map = |edits: &[[i32; 4]]| edits.iter().map(|[x, y, z, id]| (IVec3::new(*x, *y, *z), *id as VoxelId)).collect();
    match read_json::<WorldLog>(LOG_PATH) {
        Some(log) if log.seed == seed => {
            let hash = log.hash();
            let cached = cache.filter(|c| c.seed == seed && c.log_hash == Some(hash)).map(|c| to_map(&c.edits));
            if cached.is_none() {
                info!("replaying {} saved changes", log.changes.len());
            }
            (log, cached)
        }
        Some(log) => {
            warn!("ignoring {LOG_PATH}: it was made for seed {}, not {seed}", log.seed);
            (WorldLog::new(seed), None)
        }
        None => {
            let log = migrate(seed, cache.filter(|c| c.seed == seed));
            write_json(LOG_PATH, &log);
            (log, None)
        }
    }
}

/// Builds a log from the save files that came before it: voxel edits become one
/// legacy change, then each saved build is placed.
#[cfg(not(target_arch = "wasm32"))]
fn migrate(seed: u32, voxels: Option<SavedEdits>) -> WorldLog {
    /// A build as `saves/creations.json` stored it.
    #[derive(Deserialize)]
    struct OldCreation {
        id: u64,
        name: String,
        prompt: String,
        provider: String,
        position: [f32; 3],
        yaw: f32,
        spec: cubelark_rules::build::BuildSpec,
    }

    let mut log = WorldLog::new(seed);
    let profile = load_profile();
    let by = |agent: Option<&str>, prompt: Option<&str>| Provenance::new(&profile.id, &profile.name, agent, prompt, now());
    if let Some(voxels) = voxels.filter(|v| !v.edits.is_empty()) {
        log.push(by(None, None), Action::LegacyVoxels { edits: voxels.edits });
    }
    let builds: Vec<OldCreation> = read_json("saves/creations.json").unwrap_or_default();
    for c in builds {
        let by = by(Some(&c.provider), Some(&c.prompt));
        log.push(
            by,
            Action::PlaceBuild(BuildState {
                build: c.id,
                name: c.name,
                prompt: c.prompt,
                provider: c.provider,
                spec: c.spec,
                position: c.position,
                yaw: c.yaw,
            }),
        );
    }
    if !log.changes.is_empty() {
        info!("migrated old saves into {LOG_PATH} ({} changes)", log.changes.len());
    }
    for old in ["saves/creations.json", "saves/terrain_changes.json"] {
        if std::path::Path::new(old).exists() {
            let _ = std::fs::rename(old, format!("{old}.bak"));
        }
    }
    log
}
