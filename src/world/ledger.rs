//! The world's change log in the game ([`cubelark_rules::log`]): loading it, recording
//! changes with who made them, and saving.
//!
//! On desktop the log is `saves/world.json`. Replaying it is the truth;
//! `saves/world_edits.json` is a cache of the replayed voxels, used when its hash
//! matches the log. Saves from before the log are migrated once (the old files are
//! kept as `.bak`). The local player is `saves/profile.json`.
//!
//! The web build has no saves and no builder yet, so nothing records there.
#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

use std::collections::{BTreeMap, HashMap};

use bevy::prelude::*;
use cubelark_rules::log::{Action, BuildState, Provenance, WorldLog, replay};
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
        self.save();
        id
    }

    /// Undoes change `id` by appending a revert of it.
    pub fn revert(&mut self, change: u64) {
        self.record(None, None, Action::Revert { change });
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
    let (log, cached) = load_log(seed);
    let replayed = replay(&log);
    for problem in &replayed.problems {
        warn!("saved change no longer applies: {problem}");
    }
    // Builds always come from the replay (cheap); voxels from the cache when it's current.
    let edits = cached.unwrap_or(replayed.edits);
    let ledger = Ledger { log, profile, builds: replayed.builds, version: 1 };
    (ledger, edits)
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
