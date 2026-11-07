//! The world's change log in the game ([`cubelark_rules::log`]): loading it, recording
//! changes with who made them, and saving.
//!
//! On desktop the log is `saves/world.json`. Replaying it is the truth;
//! `saves/world_edits.json` is a cache of the replayed voxels, used when its hash
//! matches the log. Saves from before the log are migrated once (the old files are
//! kept as `.bak`). The local player is `saves/profile.json`: a name and a secret key
//! whose public half is the player's id, and every change is signed with it. The
//! world's first player is its admin. `CUBELARK_AS=Name` plays as a different
//! (non-admin) player for this run, to try the rules from someone else's side.
//!
//! Online (`CUBELARK_SERVER=host:port`), the server's log is the world: changes are
//! signed and proposed to it instead of recorded here, and what it commits (anyone's)
//! comes back to be applied. Local saves aren't touched while online.
//!
//! The web build has no saves and no builder yet, so nothing records there.
#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

use std::collections::{BTreeMap, HashMap};
#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashSet;

use bevy::prelude::*;
use cubelark_rules::{
    budget::{Budgets, Footprint, Usage},
    identity::Keypair,
    land::Land,
    rate::Rates,
    settings::WorldRules,
    log::{Action, BuildState, Provenance, Replay, WorldLog, replay, replay_land, signing_bytes},
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
    /// The public key, in hex.
    pub id: String,
    pub name: String,
    /// The secret key, in hex. Profiles from before keys have none and get one.
    #[serde(default)]
    pub secret: Option<String>,
    /// A `CUBELARK_AS` guest, never saved.
    #[serde(skip)]
    pub guest: bool,
}

#[derive(Resource)]
pub struct Ledger {
    pub log: WorldLog,
    pub profile: Profile,
    key: Keypair,
    /// Placed builds as of loading, for the builder to spawn.
    pub builds: BTreeMap<u64, BuildState>,
    /// Who owns what, as of the latest change.
    pub land: Land,
    /// The world's rules in force now.
    pub rules: WorldRules,
    /// Who changed how much recently, for rate limits.
    pub rates: Rates,
    /// What's used of each parcel's and player's budget.
    pub budgets: Budgets,
    /// Each placed build's share of the budgets.
    pub footprints: HashMap<u64, Footprint>,
    /// Bumped on every recorded change, so the voxel cache knows to follow.
    pub version: u64,
    /// The server this world lives on, when playing online.
    #[cfg(not(target_arch = "wasm32"))]
    pub online: Option<Online>,
}

/// A connection to a server and what's in flight on it.
#[cfg(not(target_arch = "wasm32"))]
pub struct Online {
    pub addr: String,
    pub client: cubelark_net::Client,
    /// The world as the server has committed it (voxels included, so remote changes
    /// apply one at a time). None until the server welcomes us.
    pub state: Option<Replay>,
    next_request: u64,
    /// Our proposals the server hasn't answered yet.
    pub pending: HashSet<u64>,
    /// Our proposals the server committed: request → change id.
    pub committed: HashMap<u64, u64>,
    /// Why the connection ended, once it has.
    pub closed: Option<String>,
}

/// Online, changes are known by request number until the server commits them; these
/// start high so they never look like change ids.
#[cfg(not(target_arch = "wasm32"))]
const FIRST_REQUEST: u64 = 1 << 40;

impl Ledger {
    /// Appends a change made by the local player (through `agent`, if one did it) and
    /// saves the log. Returns the change's id. Online, the change is proposed to the
    /// server instead, and the returned reference is its request number until the
    /// server commits it ([`Self::resolve`]).
    pub fn record(&mut self, agent: Option<&str>, prompt: Option<&str>, action: Action) -> u64 {
        let by = Provenance::new(&self.profile.id, &self.profile.name, agent, prompt, now());
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(online) = &mut self.online {
            let sig = self.key.sign(&signing_bytes(&by, &action));
            let request = online.next_request;
            online.next_request += 1;
            online.pending.insert(request);
            online.client.send(cubelark_net::ToServer::Propose { request, by, action, sig });
            return request;
        }
        self.log.push_signed(by, action, &self.key);
        let id = self.log.changes.last().map_or(0, |c| c.id);
        self.adopt(&replay_land(&self.log));
        self.save();
        id
    }

    /// Takes on who-owns-what, rules, rates and budgets from a replayed world.
    pub fn adopt(&mut self, state: &Replay) {
        self.land = state.land.clone();
        self.rules = state.rules.clone();
        self.rates = state.rates.clone();
        self.budgets = state.budgets.clone();
        self.footprints = state.footprints.clone();
        self.builds = state.builds.clone();
        self.version += 1;
    }

    /// The change id a reference from [`Self::record`] stands for: itself offline,
    /// and online once the server has committed it.
    pub fn resolve(&self, reference: u64) -> Option<u64> {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(online) = &self.online {
            return if reference >= FIRST_REQUEST { online.committed.get(&reference).copied() } else { Some(reference) };
        }
        Some(reference)
    }

    pub fn is_online(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        return self.online.is_some();
        #[cfg(target_arch = "wasm32")]
        false
    }

    /// Whether the server's rate limits let the local player make a change of `cost`
    /// voxels now (admins are exempt).
    pub fn check_rate(&self, cost: usize) -> Result<(), String> {
        if self.land.is_admin(&self.profile.id) {
            return Ok(());
        }
        self.rates.check(&self.rules.rates, &self.profile.id, now(), cost)
    }

    /// Whether the local player may change `parcels`; returns the free ones the change
    /// would claim, or why not.
    pub fn check(&self, parcels: &[IVec2]) -> Result<Vec<IVec2>, String> {
        self.land.authorize(&self.profile.id, parcels)
    }

    /// Undoes the change `reference` stands for by appending (or, online, proposing) a
    /// revert of it.
    pub fn revert(&mut self, reference: u64) {
        match self.resolve(reference) {
            Some(change) => {
                self.record(None, None, Action::Revert { change });
            }
            None => warn!("can't undo a change the server hasn't confirmed yet"),
        }
    }

    /// The fullest of `parcels`, as "parcel budget: 2/12 builds · 18k/40k faces · …".
    pub fn parcel_line(&self, parcels: &[IVec2]) -> String {
        let fullest = parcels
            .iter()
            .map(|p| self.budgets.parcel(*p))
            .max_by_key(|u| u.faces * 1000 / self.rules.budgets.parcel_faces.max(1) + u.builds as usize * 1000 / self.rules.budgets.parcel_builds as usize)
            .unwrap_or_default();
        format!(
            "Parcel budget: {}/{} builds · {}/{} faces · {}/{} glow",
            fullest.builds,
            self.rules.budgets.parcel_builds,
            short(fullest.faces),
            short(self.rules.budgets.parcel_faces),
            short(fullest.glow),
            short(self.rules.budgets.parcel_glow)
        )
    }

    /// The local player's totals.
    pub fn player_line(&self) -> String {
        let u: Usage = self.budgets.player(&self.profile.id);
        format!(
            "Your budget: {}/{} builds · {}/{} build voxels · {}/{} terrain voxels",
            u.builds,
            self.rules.budgets.player_builds,
            short(u.voxels),
            short(self.rules.budgets.player_voxels),
            short(u.terrain),
            short(self.rules.budgets.player_terrain)
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
        if !self.profile.guest {
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
        if self.online.is_none() {
            write_json(LOG_PATH, &self.log);
        }
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
    let (profile, key, old_id) = load_profile();
    // The local world is upgraded along with the profile even when playing online, so
    // it never names a player id the profile no longer has.
    let (mut log, cached) = load_log(seed);
    // The world's first player runs it.
    let mut changed = log.admins.is_empty();
    if changed {
        log.admins.push(profile.id.clone());
    }
    // Upgrading to keys: the old id becomes the key everywhere, and the player's own
    // unsigned changes get signed.
    if let Some(old) = &old_id {
        for admin in log.admins.iter_mut().filter(|a| *a == old) {
            *admin = profile.id.clone();
        }
    }
    for change in &mut log.changes {
        if old_id.as_ref() == Some(&change.by.player) {
            change.by.player = profile.id.clone();
        }
        if change.by.player == profile.id && change.sig.is_none() {
            change.sig = Some(key.sign(&signing_bytes(&change.by, &change.action)));
            changed = true;
        }
    }
    if changed {
        #[cfg(not(target_arch = "wasm32"))]
        write_json(LOG_PATH, &log);
    }
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(online) = connect(&profile, &key) {
        // The server sends the world once we've logged in; until then, just the seed.
        let (profile, key) = guest().unwrap_or((profile, key));
        let mut ledger = empty(WorldLog::new(seed), profile, key);
        ledger.online = Some(online);
        return (ledger, HashMap::new());
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
    let (profile, key) = guest().unwrap_or((profile, key));
    let mut ledger = empty(log, profile, key);
    ledger.adopt(&replayed);
    (ledger, edits)
}

fn empty(log: WorldLog, profile: Profile, key: Keypair) -> Ledger {
    Ledger {
        log,
        profile,
        key,
        builds: default(),
        land: default(),
        rules: default(),
        rates: default(),
        budgets: default(),
        footprints: default(),
        version: 0,
        #[cfg(not(target_arch = "wasm32"))]
        online: None,
    }
}

/// `CUBELARK_SERVER=host:port`: joins that server instead of the local world. If it
/// can't be reached, the local world loads instead.
#[cfg(not(target_arch = "wasm32"))]
fn connect(profile: &Profile, key: &Keypair) -> Option<Online> {
    let addr = std::env::var("CUBELARK_SERVER").ok().filter(|a| !a.trim().is_empty())?;
    let (profile, key) = guest().unwrap_or((profile.clone(), key.clone()));
    let addr = if addr.contains(':') { addr } else { format!("{addr}:{}", cubelark_net::DEFAULT_PORT) };
    let signer = key.clone();
    match cubelark_net::Client::connect(&addr, profile.id.clone(), profile.name.clone(), move |m| signer.sign(m)) {
        Ok(client) => {
            info!("joining {addr} as {}", profile.name);
            Some(Online {
                addr,
                client,
                state: None,
                next_request: FIRST_REQUEST,
                pending: HashSet::new(),
                committed: HashMap::new(),
                closed: None,
            })
        }
        Err(e) => {
            error!("{e}; playing the local world instead");
            None
        }
    }
}

/// `CUBELARK_AS=Name`: a separate player for this run (not saved, never an admin). Its
/// key comes from the name, so the same guest is the same player every run.
fn guest() -> Option<(Profile, Keypair)> {
    #[cfg(target_arch = "wasm32")]
    return None;
    #[cfg(not(target_arch = "wasm32"))]
    std::env::var("CUBELARK_AS").ok().filter(|n| !n.trim().is_empty()).map(|name| {
        info!("playing as {name}");
        let tag = format!("cubelark guest {}", name.trim().to_lowercase());
        let mut seed = [0; 32];
        for (i, chunk) in seed.chunks_mut(8).enumerate() {
            chunk.copy_from_slice(&cubelark_rules::log::fnv1a(format!("{tag} {i}").as_bytes()).to_le_bytes());
        }
        let key = Keypair::from_seed(seed);
        let profile = Profile { id: key.player_id(), name: name.trim().into(), secret: None, guest: true };
        (profile, key)
    })
}

/// The web build has no saves; its player is the same every time.
#[cfg(target_arch = "wasm32")]
fn load_profile() -> (Profile, Keypair, Option<String>) {
    let key = Keypair::from_seed([1; 32]);
    (Profile { id: key.player_id(), name: "Player".into(), secret: None, guest: false }, key, None)
}

/// The local player and their key, creating them on first run. The third value is the
/// id a profile from before keys had, so the log can be moved over to the key.
#[cfg(not(target_arch = "wasm32"))]
fn load_profile() -> (Profile, Keypair, Option<String>) {
    let saved: Option<Profile> = read_json(PROFILE_PATH);
    if let Some(p) = &saved
        && let Some(key) = p.secret.as_deref().and_then(Keypair::from_secret_hex)
    {
        return (Profile { id: key.player_id(), ..p.clone() }, key, None);
    }
    let mut seed = [0; 32];
    getrandom::fill(&mut seed).expect("the OS provides randomness");
    let key = Keypair::from_seed(seed);
    let name = saved.as_ref().map_or("Player".into(), |p| p.name.clone());
    let profile = Profile { id: key.player_id(), name, secret: Some(key.secret_hex()), guest: false };
    write_json(PROFILE_PATH, &profile);
    (profile, key, saved.map(|p| p.id))
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
    let (profile, _, _) = load_profile();
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
