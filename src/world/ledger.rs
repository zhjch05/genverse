//! The world's change log in the game ([`genverse_rules::log`]): loading it, recording
//! changes with who made them, and saving.
//!
//! Worlds belong to games (`docs/worlds.md`): `saves/games/<game>/game.json` says which
//! worlds the game has and who runs it, and each world keeps its log in
//! `saves/games/<game>/worlds/<world>/world.jsonl`, append-only
//! ([`genverse_rules::store`]): a change is saved by writing just that change.
//! Replaying it is the truth; `snapshot.json` beside it holds the whole world as of
//! some change, so loading replays only what came after. Changes made here, undos and
//! loads included, are applied on top of the current state, never by replaying
//! everything. A save keeps the world as of it in `states/<change>.json`, so loading it
//! swaps that in. `GENVERSE_GAME` and `GENVERSE_WORLD` pick where to start (the
//! game `local` and its first world by default; a world named there that the game
//! doesn't have is made, flat and empty). Saves from before games move into `local`
//! once, and saves from before the log are migrated (the old files are kept as
//! `.bak`). The local player is `saves/profile.json`: a name and a secret key
//! whose public half is the player's id, and every change is signed with it. The
//! world's first player is its admin. `GENVERSE_AS=Name` plays as a different
//! (non-admin) player for this run, to try the rules from someone else's side.
//!
//! Online (`GENVERSE_SERVER=host:port`), the server's log is the world: changes are
//! signed and proposed to it instead of recorded here, and what it commits (anyone's)
//! comes back to be applied. Local saves aren't touched while online.
//!
//! The web build has no saves and no builder yet, so nothing records there.
#![cfg_attr(target_arch = "wasm32", allow(dead_code, unused_imports, unused_mut, unused_assignments))]

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};
#[cfg(not(target_arch = "wasm32"))]
use std::net::SocketAddr;

use bevy::prelude::*;
use genverse_rules::{
    budget::{Budgets, Footprint, Usage},
    identity::Keypair,
    land::Land,
    rate::Rates,
    settings::WorldRules,
    log::{Action, BuildState, PortalTo, Provenance, Replay, Snapshot, WorldLog, WorldState, replay_from, signing_bytes},
    undo::{Cell, Op, UndoRecord},
    game::{GameAction, GameLog},
    logic::{Clockwork, Ground, Intent, Logic, Package, Scope},
};
use serde::{Deserialize, Serialize};

use crate::voxel::VoxelId;

/// The game and world being played, once chosen.
static CURRENT: std::sync::RwLock<Option<(String, String)>> = std::sync::RwLock::new(None);

pub use genverse_rules::game::clean_name;

/// Where games are kept.
const GAMES: &str = "saves/games";

/// The game and world being played. At first they're `GENVERSE_GAME` (or `local`) and
/// `GENVERSE_WORLD` (or the game's first world); saves from before games move into
/// `local` then.
pub fn place() -> (String, String) {
    if let Some(place) = CURRENT.read().ok().and_then(|c| c.clone()) {
        return place;
    }
    #[cfg(not(target_arch = "wasm32"))]
    migrate_layout();
    let game = std::env::var("GENVERSE_GAME").ok().and_then(|n| clean_name(&n)).unwrap_or("local".into());
    let log = read_game(&game);
    let world = std::env::var("GENVERSE_WORLD")
        .ok()
        .and_then(|n| clean_name(&n))
        .or_else(|| log.start())
        .unwrap_or("main".into());
    // A world named at start that the game doesn't have yet is made, flat and empty.
    #[cfg(not(target_arch = "wasm32"))]
    if log.world(&world).is_none() {
        let mut log = log;
        let by = Provenance::new("local", "local", None, None, now());
        let _ = log.push(by, GameAction::CreateWorld { name: world.clone(), seed: crate::world::SEED, flat: true });
        write_game(&game, &log);
    }
    set_place(&game, &world);
    (game, world)
}

pub fn game_name() -> String {
    place().0
}

pub fn world_name() -> String {
    place().1
}

/// Switches which game and world [`world_dir`] means.
pub fn set_place(game: &str, world: &str) {
    if let Ok(mut current) = CURRENT.write() {
        *current = Some((game.to_string(), world.to_string()));
    }
}

fn game_dir_of(game: &str) -> std::path::PathBuf {
    std::path::Path::new(GAMES).join(game)
}

/// Where the current world's log and caches are kept.
pub fn world_dir() -> String {
    let (game, world) = place();
    game_dir_of(&game).join("worlds").join(world).to_string_lossy().into()
}

/// The current game's own directory (`game.json`, and `worlds/`): what hosting serves.
pub fn game_dir() -> String {
    game_dir_of(&game_name()).to_string_lossy().into()
}

/// `game`'s log (an empty one if it has none yet).
#[cfg(not(target_arch = "wasm32"))]
pub fn read_game(game: &str) -> GameLog {
    match genverse_rules::store::read_game(&game_dir_of(game)) {
        Ok(Some(log)) => log,
        Ok(None) => GameLog::new(game),
        Err(e) => {
            error!("couldn't read game {game}: {e}");
            GameLog::new(game)
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub fn read_game(game: &str) -> GameLog {
    let mut log = GameLog::new(game);
    let by = Provenance::new("local", "local", None, None, 0);
    let _ = log.push(by, GameAction::CreateWorld { name: "home".into(), seed: crate::world::SEED, flat: false });
    log
}

#[cfg(not(target_arch = "wasm32"))]
fn write_game(game: &str, log: &GameLog) {
    if let Err(e) = genverse_rules::store::write_game(&game_dir_of(game), log) {
        error!("couldn't save game {game}: {e}");
    }
}

#[cfg(target_arch = "wasm32")]
fn write_game(_game: &str, _log: &GameLog) {}

/// Moves saves from before games into the game `local`, once: Home (the files in
/// `saves/`) becomes its world `home`, and each `saves/worlds/<name>` its world
/// `<name>`. Nothing is deleted.
#[cfg(not(target_arch = "wasm32"))]
fn migrate_layout() {
    use std::path::Path;
    if Path::new(GAMES).exists() {
        return;
    }
    let local = game_dir_of("local");
    let mut game = GameLog::new("local");
    let (profile, _, _) = load_profile();
    let by = || Provenance::new(&profile.id, &profile.name, None, None, now());
    let home = local.join("worlds").join("home");
    let _ = std::fs::create_dir_all(&home);
    let old_home = ["world.jsonl", "world.json", "world.json.bak", "snapshot.json"].iter().any(|f| Path::new("saves").join(f).exists());
    if old_home {
        for file in ["world.jsonl", "world.json", "world.json.bak", "snapshot.json", "world_edits.json"] {
            let from = Path::new("saves").join(file);
            if from.exists() {
                let _ = std::fs::rename(&from, home.join(file));
            }
        }
    } else {
        // Saves from before the log, or a first run: Home starts from them.
        let cache = read_json::<SavedEdits>(OLD_EDITS).filter(|c| c.seed == crate::world::SEED);
        let log = migrate(crate::world::SEED, cache);
        if let Ok((mut file, _)) = genverse_rules::store::LogFile::open(&home) {
            let _ = file.rewrite(&log);
        }
    }
    let home_log = genverse_rules::store::LogFile::open(&home).ok().and_then(|(_, log)| log);
    let admins = home_log.as_ref().map(|l| l.admins.clone()).filter(|a| !a.is_empty()).unwrap_or(vec![profile.id.clone()]);
    let _ = game.push(by(), GameAction::SetAdmins { admins });
    let (seed, flat) = home_log.map_or((crate::world::SEED, false), |l| (l.seed, l.flat));
    let _ = game.push(by(), GameAction::CreateWorld { name: "home".into(), seed, flat });
    for entry in std::fs::read_dir("saves/worlds").into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(clean) = clean_name(&name).filter(|c| *c == name && *c != "home") else { continue };
        let to = local.join("worlds").join(&clean);
        if std::fs::rename(entry.path(), &to).is_err() {
            continue;
        }
        let Some(summary) = genverse_rules::store::summary(&to) else { continue };
        let _ = game.push(by(), GameAction::CreateWorld { name: clean, seed: summary.seed, flat: summary.flat });
    }
    let _ = std::fs::remove_dir("saves/worlds");
    write_game("local", &game);
    info!("moved your worlds into the game \"local\" ({} worlds)", game.worlds().len());
}

/// A world in the current game, for lists.
#[derive(Clone, Debug)]
pub struct WorldInfo {
    pub name: String,
    pub flat: bool,
    pub changes: usize,
    pub modified: Option<std::time::SystemTime>,
}

/// The current game's worlds, in the order they were made.
#[cfg(not(target_arch = "wasm32"))]
pub fn list_worlds() -> Vec<WorldInfo> {
    let game = game_name();
    read_game(&game)
        .worlds()
        .into_iter()
        .map(|(name, ground)| {
            let summary = genverse_rules::store::summary(&game_dir_of(&game).join("worlds").join(&name));
            WorldInfo {
                flat: ground.flat,
                changes: summary.as_ref().map_or(0, |s| s.changes),
                modified: summary.and_then(|s| s.modified),
                name,
            }
        })
        .collect()
}

#[cfg(target_arch = "wasm32")]
pub fn list_worlds() -> Vec<WorldInfo> {
    Vec::new()
}

/// A local game, for lists.
#[derive(Clone, Debug)]
pub struct GameInfo {
    pub name: String,
    pub worlds: usize,
    pub modified: Option<std::time::SystemTime>,
}

/// The local games, most recently changed first.
#[cfg(not(target_arch = "wasm32"))]
pub fn list_games() -> Vec<GameInfo> {
    let mut games: Vec<GameInfo> = std::fs::read_dir(GAMES)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let log = genverse_rules::store::read_game(&e.path()).ok().flatten()?;
            let modified = std::fs::metadata(e.path().join("game.json")).and_then(|m| m.modified()).ok();
            Some(GameInfo { name, worlds: log.worlds().len(), modified })
        })
        .collect();
    games.sort_by_key(|g| std::cmp::Reverse(g.modified));
    games
}

#[cfg(target_arch = "wasm32")]
pub fn list_games() -> Vec<GameInfo> {
    Vec::new()
}

/// Makes a new local game with one world, `main`, run by `admin`. Returns its name.
#[cfg(not(target_arch = "wasm32"))]
pub fn create_game(name: &str, flat: bool, seed: u32, admin: &Provenance) -> Result<String, String> {
    let name = clean_name(name).ok_or("A game's name is letters, digits, - and _.")?;
    if game_dir_of(&name).exists() {
        return Err(format!("There's already a game called \"{name}\"."));
    }
    let mut game = GameLog::new(&name);
    game.push(admin.clone(), GameAction::SetAdmins { admins: vec![admin.player.clone()] })?;
    game.push(admin.clone(), GameAction::CreateWorld { name: "main".into(), seed, flat })?;
    write_game(&name, &game);
    Ok(name)
}

#[cfg(target_arch = "wasm32")]
pub fn create_game(_name: &str, _flat: bool, _seed: u32, _admin: &Provenance) -> Result<String, String> {
    Err("Games can't be saved on the web yet.".into())
}

/// The voxel cache from before snapshots, read only to migrate saves from before the log.
#[cfg(not(target_arch = "wasm32"))]
const OLD_EDITS: &str = "saves/world_edits.json";

#[cfg(not(target_arch = "wasm32"))]
fn snapshot_path() -> String {
    format!("{}/snapshot.json", world_dir())
}
#[cfg(not(target_arch = "wasm32"))]
const PROFILE_PATH: &str = "saves/profile.json";

/// What the game has to redo for the world to match the ledger, offline.
#[derive(Default)]
pub struct Follow {
    /// Voxels an undo set back.
    pub cells: Vec<Cell>,
    /// Builds changed (an undo, or a load): spawn them again.
    pub builds: bool,
    /// A load: every voxel edit, `[x, y, z, block]`.
    pub edits: Option<Vec<[i32; 4]>>,
    /// Saves to keep the world's state for, by change (it takes the voxels).
    pub keep: Vec<u64>,
}

/// What an action did for the local player.
pub struct Acted {
    /// How their items changed, by item.
    pub gained: Vec<(String, i64)>,
    /// What the game told them.
    pub notes: Vec<String>,
}

/// The local player.
#[derive(Clone, Serialize, Deserialize)]
pub struct Profile {
    /// The public key, in hex.
    pub id: String,
    pub name: String,
    /// The secret key, in hex. Profiles from before keys have none and get one.
    #[serde(default)]
    pub secret: Option<String>,
    /// A `GENVERSE_AS` guest, never saved.
    #[serde(skip)]
    pub guest: bool,
}

#[derive(Resource)]
pub struct Ledger {
    pub log: WorldLog,
    pub profile: Profile,
    key: Keypair,
    /// Placed builds as of loading, for the builder to spawn.
    pub builds: genverse_rules::log::Builds,
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
    /// Game packages, what's installed where, entities and inventories.
    pub logic: Logic,
    /// Builds that are portals, and where they lead.
    pub portals: BTreeMap<u64, PortalTo>,
    /// What the latest changes did, so they can be undone.
    pub undoable: BTreeMap<u64, UndoRecord>,
    /// Offline, what the last changes did that the world's voxels and builds have to
    /// follow (undos and loads): for the game to show ([`Self::follow`]).
    pub follow: Follow,
    /// Bumped on every recorded change, so the voxel cache knows to follow.
    pub version: u64,
    /// What games said to the local player on their own (timers), for the game to show.
    pub heard: Vec<String>,
    /// Saved changes that no longer applied when the world loaded (a game calling what
    /// the engine no longer has): for the player, and the agent, to know.
    pub load_problems: Vec<String>,
    /// The world's voxel edits, copied from the voxels when they change: the ground
    /// games read (`ground(x, z)`), the same as replay's.
    pub edits: std::collections::HashMap<IVec3, VoxelId>,
    pub edits_version: u64,
    /// What games did to the voxels (builds, land), and who installed them: for the game
    /// to draw.
    pub pending_world: Vec<(String, Vec<genverse_rules::logic::WorldChange>)>,
    /// Why the last change recorded didn't apply, if it didn't.
    refused: Option<String>,
    /// Offline, which timers have run up to when; online, the server runs them.
    clock: Clockwork,
    /// Where the log is saved, offline.
    #[cfg(not(target_arch = "wasm32"))]
    file: Option<genverse_rules::store::LogFile>,
    /// The server this world lives on, when playing online.
    #[cfg(not(target_arch = "wasm32"))]
    pub online: Option<Online>,
    /// Where the server this game hosts listens, and how many players are on it, if it
    /// hosts one.
    #[cfg(not(target_arch = "wasm32"))]
    pub hosting: Option<(SocketAddr, std::sync::Arc<std::sync::atomic::AtomicUsize>)>,
}

/// A connection to a server and what's in flight on it.
#[cfg(not(target_arch = "wasm32"))]
pub struct Online {
    pub addr: String,
    pub client: genverse_net::Client,
    /// The world as the server has committed it (voxels included, so remote changes
    /// apply one at a time). None until the server welcomes us.
    pub state: Option<Replay>,
    next_request: u64,
    /// Our proposals the server hasn't answered yet, with their signatures (to find
    /// them in the log if the connection dropped before the answer came).
    pub pending: HashMap<u64, String>,
    /// Our proposals the server committed: request → change id.
    pub committed: HashMap<u64, u64>,
    /// Why the connection ended, while it's down.
    pub closed: Option<String>,
    /// Failed reconnects in a row, for backing off.
    pub attempts: u32,
    /// The game the server hosts, once it has welcomed us.
    pub game: Option<GameLog>,
    /// The world we're in (or asked to go to) in that game.
    pub world: Option<String>,
    /// The world we're shown, once its welcome came.
    pub shown: Option<String>,
    /// Coming from this world through a portal: arrive by a portal back to it.
    pub arriving_from: Option<String>,
}

/// Online, changes are known by request number until the server commits them; these
/// start high so they never look like change ids.
#[cfg(not(target_arch = "wasm32"))]
const FIRST_REQUEST: u64 = 1 << 40;

/// Offline, the game keeps the world, so timers run here, on the ground it has loaded.
struct Keeper<'a> {
    ledger: &'a mut Ledger,
    ground: &'a dyn Ground,
}

impl Ground for Keeper<'_> {
    fn heights(&self, min: [i32; 2], step: i32, count: [i32; 2]) -> Option<Vec<Option<i32>>> {
        self.ground.heights(min, step, count)
    }

    fn solids(&self, lo: [f32; 2], hi: [f32; 2]) -> Vec<genverse_rules::logic::solid::Solid<'_>> {
        genverse_rules::logic::solid::of_world(&self.ledger.builds, &self.ledger.logic, lo, hi)
    }
}

impl genverse_rules::logic::TimerHost for Keeper<'_> {
    fn logic(&self) -> &Logic {
        &self.ledger.logic
    }

    fn next_change(&self) -> u64 {
        self.ledger.log.changes.last().map_or(1, |c| c.id + 1)
    }

    fn record(&mut self, package: &str, now: u64, effects: Vec<genverse_rules::logic::Effect>) -> Result<(), String> {
        let ledger = &mut *self.ledger;
        let by = Provenance::new(&ledger.profile.id, &ledger.profile.name, None, None, now);
        ledger.log.push_signed(by, Action::Timers { package: package.into(), until: now, effects }, &ledger.key);
        let result = ledger.advance();
        if result.is_err() {
            ledger.log.changes.pop();
        }
        ledger.save();
        result
    }
}

impl Ledger {
    /// Appends a change made by the local player (through `agent`, if one did it) and
    /// saves the log. Returns the change's id. Online, the change is proposed to the
    /// server instead, and the returned reference is its request number until the
    /// server commits it ([`Self::resolve`]).
    pub fn record(&mut self, agent: Option<&str>, prompt: Option<&str>, action: Action) -> u64 {
        self.record_with(agent, prompt, action, None)
    }

    /// [`Self::record`], with the package an [`Action::InstallPackage`] names (online,
    /// it goes to the server along with the change).
    fn record_with(&mut self, agent: Option<&str>, prompt: Option<&str>, action: Action, package: Option<Package>) -> u64 {
        let by = Provenance::new(&self.profile.id, &self.profile.name, agent, prompt, now());
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(online) = &mut self.online {
            let sig = self.key.sign(&signing_bytes(&by, &action));
            let request = online.next_request;
            online.next_request += 1;
            online.pending.insert(request, sig.clone());
            online.client.send(genverse_net::ToServer::Propose { request, by, action, sig, package });
            return request;
        }
        if let Some(package) = package {
            self.log.add_package(package);
        }
        self.log.push_signed(by, action, &self.key);
        let id = self.log.changes.last().map_or(0, |c| c.id);
        if let Err(why) = self.advance() {
            warn!("change {id} doesn't apply: {why}");
            self.refused = Some(why);
        }
        self.save();
        id
    }

    /// Applies the log's last change on top of the current state, as a server does:
    /// the cost of a change never grows with the history.
    fn advance(&mut self) -> Result<(), String> {
        let Some(change) = self.log.changes.last().cloned() else { return Ok(()) };
        let mut state = Replay::new(&self.log);
        // The ground games read is the seed's with these edits, as in replay.
        state.edits = std::mem::take(&mut self.edits);
        state.builds = std::mem::take(&mut self.builds);
        state.land = std::mem::take(&mut self.land);
        state.rules = std::mem::take(&mut self.rules);
        state.rates = std::mem::take(&mut self.rates);
        state.budgets = std::mem::take(&mut self.budgets);
        state.footprints = std::mem::take(&mut self.footprints);
        state.logic = Logic { packages: self.log.packages.clone(), ..std::mem::take(&mut self.logic) };
        state.portals = std::mem::take(&mut self.portals);
        state.undoable = std::mem::take(&mut self.undoable);
        let result = state.apply(&change, false).map(|_| ());
        // What the game's world has to follow.
        if result.is_ok() {
            self.follow.cells.append(&mut state.last_cells);
            match &change.action {
                Action::Undo { .. } => self.follow.builds = true,
                Action::Load { state: Some(loaded), .. } => {
                    self.follow.builds = true;
                    self.follow.cells.clear();
                    self.follow.edits = Some(loaded.edits.clone());
                }
                Action::Save { .. } => self.follow.keep.push(change.id),
                _ => {}
            }
        }
        // What a game did to the voxels, for the game to draw (replay has no voxels here).
        if let Some(world) = state.last_world.take().filter(|(_, w)| !w.is_empty()) {
            self.pending_world.push(world);
        }
        self.edits = std::mem::take(&mut state.edits);
        self.builds = state.builds;
        self.land = state.land;
        self.rules = state.rules;
        self.rates = state.rates;
        self.budgets = state.budgets;
        self.footprints = state.footprints;
        self.logic = state.logic;
        self.portals = state.portals;
        self.undoable = state.undoable;
        self.version += 1;
        result
    }

    /// Takes on who-owns-what, rules, rates and budgets from a replayed world.
    pub fn adopt(&mut self, state: &Replay) {
        self.land = state.land.clone();
        self.rules = state.rules.clone();
        self.rates = state.rates.clone();
        self.budgets = state.budgets.clone();
        self.footprints = state.footprints.clone();
        self.builds = state.builds.clone();
        self.logic = state.logic.clone();
        self.portals = state.portals.clone();
        self.undoable = state.undoable.clone();
        self.version += 1;
    }

    /// The whole world as it is now, with the voxel `edits` the game keeps.
    pub fn world_state(&self, edits: Vec<[i32; 4]>) -> WorldState {
        let mut state = Replay::new(&self.log);
        state.builds = self.builds.clone();
        state.land = self.land.clone();
        state.rules = self.rules.clone();
        state.budgets = self.budgets.clone();
        state.footprints = self.footprints.clone();
        state.logic = self.logic.clone();
        state.portals = self.portals.clone();
        state.undoable = self.undoable.clone();
        WorldState { edits, ..state.state() }
    }

    fn is_admin(&self) -> bool {
        self.land.is_admin(&self.profile.id)
    }

    /// Installs `package` over `scope` (or replaces the installed version), after checking
    /// the rules would allow it. Returns the change's reference.
    pub fn install(&mut self, package: Package, scope: Scope, agent: Option<&str>, prompt: Option<&str>) -> Result<u64, String> {
        package.check().map_err(|e| e.join("; "))?;
        let reference = package.reference();
        let mut trial = self.logic.clone();
        trial.packages.insert(reference.hash.clone(), Arc::new(package.clone()));
        self.check(&scope.parcels())?;
        trial.install(&self.profile.id, self.is_admin(), &self.rules.logic, &reference, &scope)?;
        let action = Action::InstallPackage { package: reference.clone(), scope };
        let id = self.record_with(agent, prompt, action, Some(package.clone()));
        // The game keeps it too, so other worlds can turn it on, and those that run it
        // move to this version.
        #[cfg(not(target_arch = "wasm32"))]
        let known = match &self.online {
            Some(online) => online.game.as_ref().map(|g| g.packages()),
            None => Some(read_game(&game_name()).packages()),
        };
        #[cfg(target_arch = "wasm32")]
        let known = Some(read_game(&game_name()).packages());
        if known.and_then(|k| k.get(&package.manifest.name).map(|p| p.hash())) != Some(reference.hash) {
            let action = GameAction::AddPackage { package: Box::new(package) };
            #[cfg(not(target_arch = "wasm32"))]
            if self.online.is_some() {
                let _ = self.propose_game(action);
                return Ok(id);
            }
            if let Err(why) = self.change_game(action) {
                warn!("the game didn't take the package: {why}");
            }
        }
        Ok(id)
    }

    /// Records a change to this (local) game.
    fn change_game(&mut self, action: GameAction) -> Result<(), String> {
        let game = game_name();
        let mut game_log = read_game(&game);
        let by = Provenance::new(&self.profile.id, &self.profile.name, None, None, now());
        game_log.push(by, action)?;
        write_game(&game, &game_log);
        Ok(())
    }

    /// Changes the game's packages (admins): adds or removes one. Locally the current
    /// world follows at once; on a server, every loaded world does.
    pub fn set_game_package(&mut self, action: GameAction) -> Result<(), String> {
        #[cfg(not(target_arch = "wasm32"))]
        if self.online.is_some() {
            return self.propose_game(action).map(|_| ());
        }
        self.change_game(action)?;
        self.sync_packages();
        Ok(())
    }

    /// Brings this (local) world's packages in line with its game's: moves to the game's
    /// versions, stops what it took out, and gives the game those it doesn't have yet.
    pub fn sync_packages(&mut self) {
        if self.is_online() || !self.is_admin() {
            return;
        }
        let game = read_game(&game_name());
        for sync in game.sync(&self.logic.installed) {
            match sync {
                genverse_rules::game::Sync::Upgrade(package, scope) => {
                    let reference = package.reference();
                    self.log.packages.insert(reference.hash.clone(), package);
                    self.record(None, None, Action::InstallPackage { package: reference, scope });
                }
                genverse_rules::game::Sync::Remove(name) => {
                    self.record(None, None, Action::UninstallPackage { name });
                }
                genverse_rules::game::Sync::Adopt(name) => {
                    let found = self.logic.installed.get(&name).and_then(|i| self.logic.package(i)).cloned();
                    if let Some(found) = found {
                        let _ = self.change_game(GameAction::AddPackage { package: Box::new(found) });
                    }
                }
            }
        }
    }

    /// Does `intent`, after trying it on a copy of the game: refused, it says why and
    /// nothing is recorded. A key that changes nothing (walking into a wall) isn't
    /// recorded either.
    pub fn act(&mut self, intent: Intent) -> Result<Acted, String> {
        let mut trial = self.logic.clone();
        let next = self.log.changes.last().map_or(1, |c| c.id + 1);
        let ground = genverse_rules::log::TerrainGround { terrain: self.log.terrain(), edits: &self.edits };
        let outcome = trial.act(next, &self.profile.id, now(), self.log.seed, &self.rules.logic, &intent, &ground)?;
        let me = &self.profile.id;
        let notes = outcome.notes.into_iter().filter(|(to, _)| to == me).map(|(_, text)| text).collect();
        if outcome.effects == 0 {
            return match intent {
                // Keys and messages a game ignores are nothing to complain about.
                Intent::Key { .. } | Intent::Message { .. } | Intent::Chat { .. } => Ok(Acted { gained: Vec::new(), notes }),
                Intent::Place { .. } => Err("Nothing here plays that; its game rules aren't installed (/packages)".into()),
                _ => Err("Nothing happened.".into()),
            };
        }
        let before: BTreeMap<&str, u32> = self.logic.state.inventory(me).collect();
        let after: BTreeMap<&str, u32> = trial.state.inventory(me).collect();
        let items = before.keys().chain(after.keys()).collect::<std::collections::BTreeSet<_>>();
        let gained = items
            .into_iter()
            .map(|item| (item.to_string(), *after.get(item).unwrap_or(&0) as i64 - *before.get(item).unwrap_or(&0) as i64))
            .filter(|(_, n)| *n != 0)
            .collect();
        self.refused = None;
        self.record(None, None, Action::Intent { intent });
        // The world may refuse what the game does to it (land, budgets): then it's not kept.
        if let Some(why) = self.refused.take() {
            self.log.changes.pop();
            self.save();
            return Err(why);
        }
        Ok(Acted { gained, notes })
    }

    /// Offline, fires the games' timers that are due and records what each package's
    /// did, as a server would. Timers anchored to game pieces run only while one of
    /// `players` (the hero) is near; pieces walk over `ground`. Returns the timers that
    /// failed (and stopped).
    pub fn run_timers(&mut self, players: &[(String, [f32; 3])], ground: &dyn Ground) -> Vec<String> {
        if self.is_online() || !self.is_admin() {
            return Vec::new();
        }
        let now = now();
        let mut clock = std::mem::take(&mut self.clock);
        let (seed, rules) = (self.log.seed, self.rules.logic);
        let run = clock.run(&mut Keeper { ledger: self, ground }, now, seed, &rules, players);
        self.clock = clock;
        let me = &self.profile.id;
        self.heard.extend(run.notes.into_iter().filter(|(to, _)| to == me).map(|(_, t)| t));
        run.errors
    }

    /// Adds a world to this (local) game: flat and empty, or natural from `seed`, or
    /// starting from `log` (a fork). Returns its name.
    pub fn create_world(&mut self, name: &str, ground: (u32, bool), log: Option<WorldLog>) -> Result<String, String> {
        let name = clean_name(name).ok_or("A world's name is letters, digits, - and _.")?;
        let game = game_name();
        let mut game_log = read_game(&game);
        let (seed, flat) = log.as_ref().map_or(ground, |l| (l.seed, l.flat));
        let by = Provenance::new(&self.profile.id, &self.profile.name, None, None, now());
        game_log.push(by, GameAction::CreateWorld { name: name.clone(), seed, flat })?;
        #[cfg(not(target_arch = "wasm32"))]
        {
            let dir = game_dir_of(&game).join("worlds").join(&name);
            if dir.exists() {
                return Err(format!("There's a folder for \"{name}\" already ({}).", dir.display()));
            }
            let log = log.unwrap_or(WorldLog { flat, ..WorldLog::new(seed) });
            let (mut file, _) = genverse_rules::store::LogFile::open(&dir).map_err(|e| e.to_string())?;
            file.rewrite(&log).map_err(|e| format!("Couldn't write {}: {e}", dir.display()))?;
        }
        write_game(&game, &game_log);
        Ok(name)
    }

    /// Takes a world out of this (local) game; its files move to `saves/trash/`.
    pub fn remove_world(&mut self, name: &str) -> Result<(), String> {
        if world_name() == name {
            return Err("You're in that world; go to another one first.".into());
        }
        let game = game_name();
        let mut game_log = read_game(&game);
        let by = Provenance::new(&self.profile.id, &self.profile.name, None, None, now());
        game_log.push(by, GameAction::RemoveWorld { name: name.into() })?;
        #[cfg(not(target_arch = "wasm32"))]
        {
            let trash = std::path::Path::new("saves/trash");
            std::fs::create_dir_all(trash).map_err(|e| e.to_string())?;
            let from = game_dir_of(&game).join("worlds").join(name);
            std::fs::rename(&from, trash.join(format!("{game}-{name}-{}", now()))).map_err(|e| e.to_string())?;
        }
        write_game(&game, &game_log);
        Ok(())
    }

    /// Saves every world of this (local) game at once.
    pub fn save_game(&mut self, name: &str) -> Result<usize, String> {
        let game = game_name();
        let mut game_log = read_game(&game);
        #[cfg(not(target_arch = "wasm32"))]
        let worlds: BTreeMap<String, u64> = game_log
            .worlds()
            .into_iter()
            .map(|(world, _)| {
                // Change ids count up from 1, so the last is how many there are.
                let last = if world == world_name() {
                    self.log.changes.last().map_or(0, |c| c.id)
                } else {
                    genverse_rules::store::summary(&game_dir_of(&game).join("worlds").join(&world)).map_or(0, |s| s.changes as u64)
                };
                (world, last)
            })
            .collect();
        #[cfg(target_arch = "wasm32")]
        let worlds = BTreeMap::new();
        let n = worlds.len();
        // Each world keeps its state as of the save, so loading it never replays: this
        // one once the game has its voxels, the others now.
        #[cfg(not(target_arch = "wasm32"))]
        for (world, last) in &worlds {
            if *world == world_name() {
                self.follow.keep.push(*last);
                continue;
            }
            let dir = game_dir_of(&game).join("worlds").join(world);
            let Ok((_, Some(log))) = genverse_rules::store::LogFile::open(&dir) else { continue };
            let snapshot = read_snapshot(&dir.join("snapshot.json"));
            let state = genverse_rules::store::state_of_save(&dir, &log, *last, snapshot.as_ref());
            if let Err(e) = genverse_rules::store::write_state(&dir, &Snapshot::at(&log, *last, state)) {
                warn!("couldn't keep {world}'s save: {e}");
            }
        }
        let by = Provenance::new(&self.profile.id, &self.profile.name, None, None, now());
        game_log.push(by, GameAction::Save { name: name.trim().into(), worlds })?;
        write_game(&game, &game_log);
        Ok(n)
    }

    /// Loads a game save: every world goes back to how it was then (admins). Locally,
    /// the current world's restore is recorded like any change and the others' are
    /// written to their logs, for when they're entered next. On a server, the admin signs
    /// each world's restore and the server makes them.
    pub fn load_game(&mut self, name: &str, edits: &[[i32; 4]]) -> Result<usize, String> {
        if !self.is_admin() {
            return Err("Only admins load saves.".into());
        }
        self.keep_saves(edits);
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(online) = &self.online {
            let game = online.game.as_ref().ok_or("the server hasn't welcomed us yet")?;
            let worlds = game.save_named(name).ok_or(format!("There's no game save \"{name}\"."))?.clone();
            let by = Provenance::new(&self.profile.id, &self.profile.name, None, None, now());
            let signed = worlds
                .iter()
                .map(|(world, to)| (world.clone(), self.key.sign(&signing_bytes(&by, &genverse_rules::game::load_action(name, *to)))))
                .collect();
            self.propose_game_by(by, GameAction::LoadSave { name: name.into(), signed })?;
            return Ok(worlds.len());
        }
        self.change_game(GameAction::LoadSave { name: name.into(), signed: BTreeMap::new() })?;
        let game = game_name();
        let game_log = read_game(&game);
        let (_, _, worlds) = game_log.saves().rev().find(|(_, n, _)| n.eq_ignore_ascii_case(name)).ok_or(format!("There's no game save \"{name}\"."))?;
        let mut n = 0;
        for (world, to) in worlds {
            if *world == world_name() {
                if self.log.changes.last().map_or(0, |c| c.id) > *to {
                    let state = Some(Box::new(self.state_of_save(*to)));
                    self.record(None, None, Action::Load { save: *to, name: name.trim().into(), state });
                    n += 1;
                }
                continue;
            }
            #[cfg(not(target_arch = "wasm32"))]
            {
                let dir = game_dir_of(&game).join("worlds").join(world);
                let Ok((mut file, Some(mut log))) = genverse_rules::store::LogFile::open(&dir) else { continue };
                if log.changes.last().map_or(0, |c| c.id) <= *to {
                    continue;
                }
                let snapshot = read_snapshot(&dir.join("snapshot.json"));
                let state = Some(Box::new(genverse_rules::store::state_of_save(&dir, &log, *to, snapshot.as_ref())));
                let by = Provenance::new(&self.profile.id, &self.profile.name, None, None, now());
                log.push_signed(by, Action::Load { save: *to, name: name.trim().into(), state }, &self.key);
                file.sync(&log).map_err(|e| e.to_string())?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// Online, goes to another world of the server's game; its welcome follows.
    /// `from_portal` is the world left through a portal, to arrive by one leading back.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn enter_remote(&mut self, world: &str, from_portal: Option<String>) -> Result<(), String> {
        let online = self.online.as_mut().ok_or("not online")?;
        let game = online.game.as_ref().ok_or("the server hasn't welcomed us yet")?;
        if game.world(world).is_none() {
            return Err(format!("There's no world \"{world}\" in {}.", game.name));
        }
        online.world = Some(world.to_string());
        online.arriving_from = from_portal;
        online.client.send(genverse_net::ToServer::Enter { world: world.to_string() });
        Ok(())
    }

    /// Online, asks the server to change its game (admins make worlds; anyone saves).
    /// Returns the request number; the game comes back changed, or a refusal.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn propose_game(&mut self, action: GameAction) -> Result<u64, String> {
        let by = Provenance::new(&self.profile.id, &self.profile.name, None, None, now());
        self.propose_game_by(by, action)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn propose_game_by(&mut self, by: Provenance, action: GameAction) -> Result<u64, String> {
        let sig = self.key.sign(&genverse_rules::game::signing_bytes(&by, &action));
        let online = self.online.as_mut().ok_or("not online")?;
        let request = online.next_request;
        online.next_request += 1;
        online.client.send(genverse_net::ToServer::ProposeGame { request, by, action, sig });
        Ok(request)
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

    /// Joins the server at `addr` (`host` or `host:port`); its world arrives as a
    /// welcome. The local world stays saved as it is.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn go_online(&mut self, addr: &str) -> Result<(), String> {
        self.online = Some(connect_to(addr, &self.profile, &self.key)?);
        Ok(())
    }

    /// Connects again to the server we lost, keeping what's in flight: the welcome that
    /// follows settles it.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn reconnect(&mut self) -> Result<(), String> {
        let online = self.online.as_mut().ok_or("not online")?;
        let fresh = connect_to(&online.addr, &self.profile, &self.key)?;
        online.client = fresh.client;
        online.state = None;
        online.closed = None;
        Ok(())
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
        if !self.is_admin() && !self.logic.building_allowed() {
            return Err("This world is a game right now: only admins build here.".into());
        }
        self.land.authorize(&self.profile.id, parcels)
    }

    /// Undoes the change `reference` stands for (online, proposes it): what it replaced
    /// comes back on the world as it is now, or, if something changed there since, it
    /// says so and nothing happens.
    pub fn undo(&mut self, reference: u64) -> Result<(), String> {
        let change = self.resolve(reference).ok_or("the server hasn't confirmed that change yet")?;
        let record = self.undoable.get(&change).ok_or(format!("Change {change} can't be undone (it's undone already, too old, or a kind that can't be)."))?;
        if record.by != self.profile.id && !self.is_admin() {
            return Err("You can only undo your own changes.".into());
        }
        self.refused = None;
        self.record(None, None, Action::Undo { change });
        if let Some(why) = self.refused.take() {
            self.log.changes.pop();
            self.save();
            return Err(why);
        }
        Ok(())
    }

    /// The local player's latest change that can be undone (`redo`: their latest undo,
    /// which undoing redoes).
    pub fn latest_undoable(&self, redo: bool) -> Option<u64> {
        self.undoable.iter().rev().find(|(_, r)| r.by == self.profile.id && r.redo == redo).map(|(id, _)| *id)
    }

    /// Offline, the voxels land change `change` set, as the game applied them: kept to
    /// undo it (the ledger doesn't keep voxels itself).
    pub fn note_cells(&mut self, change: u64, cells: Vec<Cell>) {
        if let Some(Op::Land { cells: kept, .. }) = self.undoable.get_mut(&change).and_then(|r| r.ops.first_mut()) {
            *kept = cells;
        }
    }

    /// The world as of save `save` (its change): as it was kept, or replayed up to it.
    pub fn state_of_save(&self, save: u64) -> WorldState {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let snapshot = read_snapshot(std::path::Path::new(&snapshot_path()));
            genverse_rules::store::state_of_save(std::path::Path::new(&world_dir()), &self.log, save, snapshot.as_ref())
        }
        #[cfg(target_arch = "wasm32")]
        genverse_rules::log::state_at(&self.log, save, None)
    }

    /// Offline, keeps the states of the saves just made, with the world's voxel `edits`
    /// as they are now (the ledger doesn't keep voxels). Whoever saves calls it right
    /// after, before anything else changes.
    pub fn keep_saves(&mut self, edits: &[[i32; 4]]) {
        for save in std::mem::take(&mut self.follow.keep) {
            keep_save(self, save, edits.to_vec());
        }
    }

    /// Loads save `save` (admins): first saves the world as it is ("before loading …"),
    /// so loading that brings it back; `edits` are its voxel edits. Online, the server
    /// swaps the state in.
    pub fn load(&mut self, save: u64, edits: &[[i32; 4]]) -> Result<String, String> {
        if !self.is_admin() {
            return Err("Only admins load saves.".into());
        }
        let Some(Action::Save { name }) = self.log.change(save).map(|c| c.action.clone()) else {
            return Err(format!("Change {save} isn't a save."));
        };
        let before = format!("before loading {name}").chars().take(40).collect::<String>();
        self.record(None, None, Action::Save { name: before.clone() });
        self.keep_saves(edits);
        let state = (!self.is_online()).then(|| Box::new(self.state_of_save(save)));
        self.refused = None;
        self.record(None, None, Action::Load { save, name: name.clone(), state });
        if let Some(why) = self.refused.take() {
            self.log.changes.pop();
            self.save();
            return Err(why);
        }
        Ok(before)
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
        // A guest (GENVERSE_AS) is only for this run.
        #[cfg(not(target_arch = "wasm32"))]
        if !self.profile.guest {
            write_json(PROFILE_PATH, &self.profile);
        }
    }

    /// Who first placed build `build`, and who changed it last (if anyone did since).
    pub fn credits(&self, build: u64) -> Option<(&Provenance, Option<&Provenance>)> {
        let mut changes = self.log.changes.iter().filter(|c| match &c.action {
            Action::PlaceBuild(b) | Action::UpdateBuild(b) => b.build == build,
            _ => false,
        });
        let first = changes.next()?;
        Some((&first.by, changes.next_back().map(|c| &c.by)))
    }

    /// A short credit line: "Ada with Claude Code, 2 h ago" ("you" for the local player).
    pub fn describe(&self, by: &Provenance) -> String {
        let who = if by.player == self.profile.id { "you".to_string() } else { by.player_name.clone() };
        let how = by.agent.as_deref().map(|a| format!(" with {a}")).unwrap_or_default();
        format!("{who}{how}, {}", ago(now().saturating_sub(by.at)))
    }

    fn save(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        if self.online.is_none()
            && let Some(file) = &mut self.file
            && let Err(e) = file.sync(&self.log)
        {
            error!("couldn't save {}: {e}", file.path().display());
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

/// Saves a snapshot of the local world as of the log's last change, with its voxel
/// `edits`.
pub fn write_snapshot(ledger: &Ledger, edits: Vec<[i32; 4]>) {
    let snapshot = Snapshot::new(&ledger.log, ledger.world_state(edits));
    #[cfg(not(target_arch = "wasm32"))]
    write_json(&snapshot_path(), &snapshot);
    #[cfg(target_arch = "wasm32")]
    let _ = snapshot;
}

/// Keeps the local world's state as of save `save` (the log's last change then, so the
/// world as it is now, with its voxel `edits`): loading the save swaps it in.
pub fn keep_save(ledger: &Ledger, save: u64, edits: Vec<[i32; 4]>) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let snapshot = Snapshot::at(&ledger.log, save, ledger.world_state(edits));
        if let Err(e) = genverse_rules::store::write_state(std::path::Path::new(&world_dir()), &snapshot) {
            error!("couldn't keep the save: {e}");
        }
    }
    #[cfg(target_arch = "wasm32")]
    let _ = (ledger, save, edits);
}

#[cfg(not(target_arch = "wasm32"))]
fn write_json(path: &str, value: &impl Serialize) {
    let dir = std::path::Path::new(path).parent().unwrap_or(std::path::Path::new("."));
    let tmp = format!("{path}.tmp");
    let result = std::fs::create_dir_all(dir)
        .and_then(|_| std::fs::write(&tmp, serde_json::to_string(value).unwrap_or_default()))
        .and_then(|_| std::fs::rename(&tmp, path));
    if let Err(e) = result {
        error!("couldn't save {path}: {e}");
    }
}

/// The snapshot at `path`, if there's one. One in an older form is just skipped: the
/// next one replaces it.
#[cfg(not(target_arch = "wasm32"))]
fn read_snapshot(path: &std::path::Path) -> Option<Snapshot> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

#[cfg(not(target_arch = "wasm32"))]
fn read_json<T: serde::de::DeserializeOwned>(path: &str) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).map_err(|e| error!("couldn't read {path}: {e}")).ok()
}

/// Loads the log (or migrates old saves into one) and returns it with the voxel edits
/// it describes. With `GENVERSE_SERVER`, it joins that server instead.
pub fn load(seed: u32) -> (Ledger, HashMap<IVec3, VoxelId>) {
    load_with(seed, true)
}

/// Loads the current local world ([`world_name`]), never a server: for switching worlds.
pub fn load_local(seed: u32) -> (Ledger, HashMap<IVec3, VoxelId>) {
    load_with(seed, false)
}

fn load_with(seed: u32, join: bool) -> (Ledger, HashMap<IVec3, VoxelId>) {
    let (profile, key, old_id) = load_profile();
    // The local world is upgraded along with the profile even when playing online, so
    // it never names a player id the profile no longer has.
    let (mut log, mut file, snapshot) = load_log(seed);
    // The game's admins run every world in it; a new game's first player runs it.
    let game = game_name();
    let mut game_log = read_game(&game);
    if game_log.admins().is_empty() {
        let by = Provenance::new(&profile.id, &profile.name, None, None, now());
        let _ = game_log.push(by, GameAction::SetAdmins { admins: vec![profile.id.clone()] });
        write_game(&game, &game_log);
    }
    // A world named with `GENVERSE_WORLD` that the game didn't have joins it.
    if game_log.world(&world_name()).is_none() {
        let by = Provenance::new(&profile.id, &profile.name, None, None, now());
        if game_log.push(by, GameAction::CreateWorld { name: world_name(), seed: log.seed, flat: log.flat }).is_ok() {
            write_game(&game, &game_log);
        }
    }
    let mut changed = log.admins != game_log.admins();
    if changed {
        log.admins = game_log.admins();
    }
    if log.migrate() {
        info!("took the old undos out of {}", world_dir());
        changed = true;
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
    #[cfg(not(target_arch = "wasm32"))]
    if changed
        && let Some(file) = &mut file
        && let Err(e) = file.rewrite(&log)
    {
        error!("couldn't save {}: {e}", file.path().display());
    }
    #[cfg(not(target_arch = "wasm32"))]
    if join && let Some(online) = connect(&profile, &key) {
        // The server sends the world once we've logged in; until then, just the seed.
        let (profile, key) = guest().unwrap_or((profile, key));
        let mut ledger = empty(WorldLog::new(seed), profile, key);
        ledger.online = Some(online);
        return (ledger, HashMap::new());
    }
    // From the snapshot when it fits: only the changes after it are replayed.
    if snapshot.is_none() {
        info!("replaying {} saved changes", log.changes.len());
    }
    let mut replayed = replay_from(&log, snapshot.as_ref());
    let edits = std::mem::take(&mut replayed.edits);
    for problem in &replayed.problems {
        warn!("saved change no longer applies: {problem}");
    }
    let (profile, key) = guest().unwrap_or((profile, key));
    let mut ledger = empty(log, profile, key);
    ledger.adopt(&replayed);
    ledger.load_problems = replayed.problems.clone();
    if !replayed.problems.is_empty() {
        ledger.heard.push(format!("⚠ {} saved change(s) no longer work in this world; ask the builder to \"fix the game\"", replayed.problems.len()));
    }
    ledger.clock = Clockwork::new(ledger.logic.timers_until);
    #[cfg(not(target_arch = "wasm32"))]
    {
        ledger.file = file;
    }
    // The game's packages may have moved on while this world was away.
    ledger.sync_packages();
    #[cfg(target_arch = "wasm32")]
    let _ = file;
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
        logic: default(),
        portals: default(),
        undoable: default(),
        follow: default(),
        version: 0,
        heard: Vec::new(),
        load_problems: Vec::new(),
        edits: default(),
        edits_version: u64::MAX,
        pending_world: Vec::new(),
        refused: None,
        clock: Clockwork::default(),
        #[cfg(not(target_arch = "wasm32"))]
        file: None,
        #[cfg(not(target_arch = "wasm32"))]
        online: None,
        #[cfg(not(target_arch = "wasm32"))]
        hosting: None,
    }
}

/// `GENVERSE_SERVER=host:port`: joins that server instead of the local world. If it
/// can't be reached, the local world loads instead.
#[cfg(not(target_arch = "wasm32"))]
fn connect(profile: &Profile, key: &Keypair) -> Option<Online> {
    let addr = std::env::var("GENVERSE_SERVER").ok().filter(|a| !a.trim().is_empty())?;
    let (profile, key) = guest().unwrap_or((profile.clone(), key.clone()));
    connect_to(&addr, &profile, &key).map_err(|e| error!("{e}; playing the local world instead")).ok()
}

#[cfg(not(target_arch = "wasm32"))]
fn connect_to(addr: &str, profile: &Profile, key: &Keypair) -> Result<Online, String> {
    let addr = addr.trim();
    let addr = if addr.contains(':') { addr.to_string() } else { format!("{addr}:{}", genverse_net::DEFAULT_PORT) };
    let signer = key.clone();
    let client = genverse_net::Client::connect(&addr, profile.id.clone(), profile.name.clone(), move |m| signer.sign(m))?;
    info!("joining {addr} as {}", profile.name);
    Ok(Online {
        addr,
        client,
        state: None,
        next_request: FIRST_REQUEST,
        pending: HashMap::new(),
        committed: HashMap::new(),
        closed: None,
        attempts: 0,
        game: None,
        world: None,
        shown: None,
        arriving_from: None,
    })
}

/// `GENVERSE_AS=Name`: a separate player for this run (not saved, never an admin). Its
/// key comes from the name, so the same guest is the same player every run.
fn guest() -> Option<(Profile, Keypair)> {
    #[cfg(target_arch = "wasm32")]
    return None;
    #[cfg(not(target_arch = "wasm32"))]
    std::env::var("GENVERSE_AS").ok().filter(|n| !n.trim().is_empty()).map(|name| {
        info!("playing as {name}");
        // The game's old name: the key is derived from it, so changing it makes every guest
        // someone new who no longer owns their land.
        let tag = format!("cubelark guest {}", name.trim().to_lowercase());
        let mut seed = [0; 32];
        for (i, chunk) in seed.chunks_mut(8).enumerate() {
            chunk.copy_from_slice(&genverse_rules::log::fnv1a(format!("{tag} {i}").as_bytes()).to_le_bytes());
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

/// The log, its file, and the snapshot if it fits the log.
#[cfg(target_arch = "wasm32")]
fn load_log(seed: u32) -> (WorldLog, Option<()>, Option<Snapshot>) {
    (WorldLog::new(seed), None, None)
}

#[cfg(not(target_arch = "wasm32"))]
fn load_log(seed: u32) -> (WorldLog, Option<genverse_rules::store::LogFile>, Option<Snapshot>) {
    use genverse_rules::store::LogFile;
    let dir = world_dir();
    let (mut file, log) = match LogFile::open(std::path::Path::new(&dir)) {
        Ok(opened) => opened,
        Err(e) => {
            error!("couldn't read the world in {dir}: {e}");
            return (WorldLog::new(seed), None, None);
        }
    };
    let log = match log {
        Some(log) => log,
        // A world the game just made starts on the ground it was given.
        None => {
            let ground = read_game(&game_name()).world(&world_name());
            let (seed, flat) = ground.map_or((seed, true), |g| (g.seed, g.flat));
            info!("made world {} in {dir}", world_name());
            WorldLog { flat, ..WorldLog::new(seed) }
        }
    };
    if !file.path().exists()
        && let Err(e) = file.rewrite(&log)
    {
        error!("couldn't save {}: {e}", file.path().display());
    }
    let snapshot = read_snapshot(std::path::Path::new(&snapshot_path())).filter(|s| s.fits(&log));
    (log, Some(file), snapshot)
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
        spec: genverse_rules::build::BuildSpec,
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
        // Those saves were edits and builds on the first terrain; a first run starts
        // on the current one.
        log.terrain_version = genverse_rules::terrain::LEGACY_TERRAIN;
        info!("migrated old saves into {} ({} changes)", world_dir(), log.changes.len());
    }
    for old in ["saves/creations.json", "saves/terrain_changes.json"] {
        if std::path::Path::new(old).exists() {
            let _ = std::fs::rename(old, format!("{old}.bak"));
        }
    }
    log
}

/// Keeps the ledger's copy of the voxel edits current: the ground games read.
pub fn copy_edits(world: Res<crate::world::VoxelWorld>, mut ledger: ResMut<Ledger>) {
    if ledger.edits_version != world.edits_version {
        ledger.edits = world.edit_map();
        ledger.edits_version = world.edits_version;
    }
}
