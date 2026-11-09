//! The world as a log of changes: the seed plus every change, in order, each with who
//! made it. Applying the changes one after another builds the world: placed builds, and
//! the voxel edits on top of the seed (terrain changes compile against the ground the
//! earlier changes left). That's what a server replicates and what a world's history is.
//!
//! The log only moves forward. Undo is a change of its own ([`Action::Undo`], see
//! [`crate::undo`]) that puts back what a change replaced, on the world as it is now.
//! The whole world as of a change is a [`WorldState`]; a [`Snapshot`] keeps one, so a
//! world loads from its latest snapshot plus the changes after it, never from the start.
//!
//! Saves are bookmarks in the log ([`Action::Save`]): anyone may make one, and the host
//! keeps the world's state as of it. Loading one is an admin's [`Action::Load`], which
//! carries that state and swaps it in. Whatever came between stays in the history.
//!
//! Replay also enforces the world's rules ([`crate::settings`]) as of each change:
//! land, limits, budgets and rate limits. So a log can't hold anything its authors
//! weren't allowed to do, and ownership and budgets fall out of the log too.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

use bevy_math::{IVec2, IVec3, UVec3, Vec3};
use serde::{Deserialize, Serialize};

use crate::{
    budget::{Budgets, Footprint, Usage, build_usage},
    build::BuildSpec,
    identity::{Keypair, verify},
    land::{Land, area_parcels, build_parcels},
    land::Parcel,
    logic::{Effect, GameState, Install, Intent, Logic, Package, PackageRef, Runtime, Scope, WorldChange},
    rate::Rates,
    sculpt::{self, Area, Columns, TerrainSpec},
    settings::WorldRules,
    terrain::{Terrain, WORLD_HEIGHT},
    undo::{Cell, Op, Placed, UndoRecord},
    voxel::VoxelId,
};

/// Where a portal leads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PortalTo {
    /// A world in the same game: everyone in the game can go.
    World { name: String },
    /// A world in another of the player's local games.
    Game { game: String, world: String },
    /// Another hosted game, `host:port`.
    Server { addr: String },
}

impl PortalTo {
    /// "sokoban" (a world in this game), "game/world" (another local game), or a
    /// server's `host:port`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.is_empty() || text.len() > 100 {
            return Err("a portal leads to a world, game/world, or host:port".into());
        }
        if text.contains(':') {
            return Ok(PortalTo::Server { addr: text.into() });
        }
        let name = |part: &str| {
            crate::game::clean_name(part).filter(|n| n.eq_ignore_ascii_case(part.trim())).ok_or(format!("\"{part}\" isn't a name (letters, digits, - and _)"))
        };
        match text.split_once('/') {
            Some((game, world)) => Ok(PortalTo::Game { game: name(game)?, world: name(world)? }),
            None => Ok(PortalTo::World { name: name(text)? }),
        }
    }

    /// Where it leads, as [`Self::parse`] reads it.
    pub fn label(&self) -> String {
        match self {
            PortalTo::World { name } => name.clone(),
            PortalTo::Game { game, world } => format!("{game}/{world}"),
            PortalTo::Server { addr } => addr.clone(),
        }
    }
}

/// Who a server's own changes are by: timers going off.
pub const WORLD: &str = "world";

/// Bumped when the rules change in a way that rasterizes the same log differently.
pub const RULES_VERSION: u32 = 1;

/// Who made a change, and how.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    pub player: String,
    pub player_name: String,
    /// The agent that made it ("Claude Code", "Codex CLI"); none for a person's own
    /// action, like moving or deleting a build.
    pub agent: Option<String>,
    /// The request, kept locally. A server would keep only [`Self::prompt_hash`].
    pub prompt: Option<String>,
    pub prompt_hash: Option<String>,
    /// Unix seconds.
    pub at: u64,
}

impl Provenance {
    pub fn new(player: &str, player_name: &str, agent: Option<&str>, prompt: Option<&str>, at: u64) -> Self {
        Self {
            player: player.into(),
            player_name: player_name.into(),
            agent: agent.map(str::to_string),
            prompt: prompt.map(str::to_string),
            prompt_hash: prompt.map(|p| format!("{:016x}", fnv1a(p.as_bytes()))),
            at,
        }
    }
}

/// A placed build as of some change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BuildState {
    pub build: u64,
    pub name: String,
    /// The request that designed it, and which agent did (shown when it's selected).
    pub prompt: String,
    pub provider: String,
    pub spec: BuildSpec,
    pub position: [f32; 3],
    pub yaw: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    PlaceBuild(BuildState),
    /// Moved, turned, or revised: the build's whole new state.
    UpdateBuild(BuildState),
    RemoveBuild { build: u64 },
    Terrain {
        name: String,
        area: Area,
        spec: TerrainSpec,
        /// Voxels it changed when it was made, for budgets without re-rasterizing.
        #[serde(default)]
        changed: usize,
    },
    /// Claims free parcels (by parcel coordinates) for the author.
    ClaimParcels { parcels: Vec<[i32; 2]> },
    /// Gives up the author's parcels.
    ReleaseParcels { parcels: Vec<[i32; 2]> },
    /// Puts back what an earlier change of the same player replaced (admins may undo
    /// anyone's), if nothing has changed there since ([`crate::undo`]). Undoing an undo
    /// redoes.
    Undo { change: u64 },
    /// From logs made before undo moved forward; [`WorldLog::migrate`] takes it out.
    Revert { change: u64 },
    /// From logs made before undo moved forward; [`WorldLog::migrate`] takes it out.
    Rollback { changes: Vec<u64>, note: String },
    /// An admin changing the world's rules from here on.
    SetRules { rules: WorldRules },
    /// Voxel edits made before the log existed (hand edits), as `[x, y, z, block]`.
    LegacyVoxels { edits: Vec<[i32; 4]> },
    /// Installs a game package from [`WorldLog::packages`], or replaces the installed
    /// version (an upgrade, or going back to an older one).
    InstallPackage { package: PackageRef, scope: Scope },
    UninstallPackage { name: String },
    /// A player doing something the installed packages respond to.
    Intent { intent: Intent },
    /// A bookmark: the world as it is now, to load later ([`Action::Load`]) or to start
    /// a new world from. Whoever keeps the world keeps its state as of here.
    Save { name: String },
    /// An admin loading the save `save` (its change; `name`): the world becomes `state`,
    /// the world as of the save. The author signs it without the state, which whoever
    /// keeps the world fills in from the save.
    Load {
        save: u64,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<Box<WorldState>>,
    },
    /// From logs made before undo moved forward; [`WorldLog::migrate`] takes it out.
    Restore { to: u64 },
    /// A player leaving for another world of the game, with what they carry and their
    /// stats: they go with them ([`Action::Arrive`] there).
    Depart {
        player: String,
        to: String,
        items: Vec<(String, u32)>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        stats: Vec<(String, crate::logic::Stat)>,
    },
    /// A player arriving from another world of the game with what they carried.
    Arrive {
        player: String,
        from: String,
        items: Vec<(String, u32)>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        stats: Vec<(String, crate::logic::Stat)>,
    },
    /// Makes a placed build a portal to another world (`None` unlinks it). Whoever may
    /// change the build may do this; the link stays when the build moves or changes.
    SetPortal { build: u64, to: Option<PortalTo> },
    /// A package's timers going off, up to `until` (Unix seconds): what they did, so
    /// replay applies it without running them again. Made by the world (a server) or,
    /// offline, by an admin's game.
    Timers {
        package: String,
        until: u64,
        #[serde(default)]
        effects: Vec<Effect>,
    },
}

impl Action {
    /// Whether it's an undo from before undo moved forward (they changed which changes
    /// count): [`WorldLog::migrate`] takes them out.
    pub fn is_legacy(&self) -> bool {
        matches!(self, Action::Revert { .. } | Action::Rollback { .. } | Action::Restore { .. })
    }

    /// A few words for history lists: "placed \"Stone well\"".
    pub fn describe(&self) -> String {
        match self {
            Action::PlaceBuild(b) => format!("placed \"{}\"", b.name),
            Action::UpdateBuild(b) => format!("changed \"{}\"", b.name),
            Action::RemoveBuild { build } => format!("removed build {build}"),
            Action::Terrain { name, .. } => format!("reshaped land: \"{name}\""),
            Action::ClaimParcels { parcels } => format!("claimed {} parcel(s)", parcels.len()),
            Action::ReleaseParcels { parcels } => format!("released {} parcel(s)", parcels.len()),
            Action::Undo { change } => format!("undid change {change}"),
            Action::Revert { change } => format!("undid change {change}"),
            Action::Rollback { changes, note } => format!("rolled back {} change(s): {note}", changes.len()),
            Action::SetRules { .. } => "changed the world's rules".into(),
            Action::LegacyVoxels { edits } => format!("old hand edits ({} voxels)", edits.len()),
            Action::InstallPackage { package, scope: Scope::World } => {
                format!("installed {} v{} everywhere", package.name, package.version)
            }
            Action::InstallPackage { package, scope } => {
                format!("installed {} v{} on {} parcel(s)", package.name, package.version, scope.parcels().len())
            }
            Action::UninstallPackage { name } => format!("uninstalled {name}"),
            Action::Intent { intent: Intent::Interact { .. } } => "interacted".into(),
            Action::Intent { intent: Intent::UseItem { item, .. } } => format!("used {item}"),
            Action::Intent { intent: Intent::Place { item, .. } } => format!("placed {item}"),
            Action::Intent { intent: Intent::Key { key, .. } } => format!("pressed {key}"),
            Action::Intent { intent: Intent::Choose { choice: Some(choice) } } => format!("chose {choice}"),
            Action::Intent { intent: Intent::Choose { choice: None } } => "closed a panel".into(),
            Action::Intent { intent: Intent::Hit { .. } } => "attacked".into(),
            Action::Timers { package, effects, .. } => format!("{package}'s timers ({} change(s))", effects.len()),
            Action::Save { name } => format!("saved \"{name}\""),
            Action::Depart { to, items, .. } => format!("left for {to} carrying {} kind(s) of item", items.len()),
            Action::Arrive { from, items, .. } => format!("arrived from {from} carrying {} kind(s) of item", items.len()),
            Action::Load { name, .. } => format!("loaded \"{name}\""),
            Action::Restore { to } => format!("restored the world to change {to}"),
            Action::SetPortal { build, to: Some(to) } => format!("made build {build} a portal to {}", to.label()),
            Action::SetPortal { build, to: None } => format!("unlinked the portal on build {build}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub id: u64,
    pub by: Provenance,
    pub action: Action,
    /// The author's signature over [`signing_bytes`], when `by.player` is a key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

/// What a change's author signs: who, how, when, and what, but not the request text
/// (which stays on their machine), the id (which the server assigns), or the state a
/// load brings (which the host fills in).
pub fn signing_bytes(by: &Provenance, action: &Action) -> Vec<u8> {
    let by = Provenance { prompt: None, ..by.clone() };
    match action {
        Action::Load { save, name, state: Some(_) } => {
            let action = Action::Load { save: *save, name: name.clone(), state: None };
            serde_json::to_vec(&(by, action)).unwrap_or_default()
        }
        _ => serde_json::to_vec(&(by, action)).unwrap_or_default(),
    }
}

impl Change {
    /// Whether the change carries a valid signature by its author.
    pub fn verified(&self) -> bool {
        self.sig.as_deref().is_some_and(|sig| verify(&self.by.player, &signing_bytes(&self.by, &self.action), sig))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorldLog {
    pub seed: u32,
    pub rules_version: u32,
    /// Players who aren't bound by the land rules or rate limits, and who may change
    /// the rules and roll back anyone's changes.
    #[serde(default)]
    pub admins: Vec<String>,
    /// The rules the world started with; admins' `SetRules` changes replace them.
    #[serde(default)]
    pub rules: WorldRules,
    /// Flat and empty ground instead of the natural landscape.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub flat: bool,
    pub changes: Vec<Change>,
    /// Game packages, by hash: everything an [`Action::InstallPackage`] may name.
    /// Packages that were never installed are harmless; they just wait.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub packages: BTreeMap<String, Arc<Package>>,
}

impl WorldLog {
    pub fn new(seed: u32) -> Self {
        Self {
            seed,
            rules_version: RULES_VERSION,
            admins: Vec::new(),
            rules: WorldRules::default(),
            flat: false,
            changes: Vec::new(),
            packages: BTreeMap::new(),
        }
    }

    /// The ground this world is made of.
    pub fn terrain(&self) -> Terrain {
        Terrain { seed: self.seed, flat: self.flat }
    }

    /// Keeps `package` so changes can install it. Returns its hash.
    pub fn add_package(&mut self, package: Package) -> String {
        let hash = package.hash();
        self.packages.entry(hash.clone()).or_insert_with(|| Arc::new(package));
        hash
    }

    /// Appends a change and returns its id.
    pub fn push(&mut self, by: Provenance, action: Action) -> u64 {
        let id = self.changes.last().map_or(1, |c| c.id + 1);
        self.changes.push(Change { id, by, action, sig: None });
        id
    }

    /// Appends a change signed with `key` (whose player id should be `by.player`).
    pub fn push_signed(&mut self, by: Provenance, action: Action, key: &Keypair) -> u64 {
        let sig = key.sign(&signing_bytes(&by, &action));
        let id = self.push(by, action);
        self.changes.last_mut().expect("just pushed").sig = Some(sig);
        id
    }

    pub fn is_admin(&self, player: &str) -> bool {
        self.admins.iter().any(|a| a == player)
    }

    /// Changes an undo took back that nothing has redone since: for history lists.
    pub fn undone(&self) -> HashSet<u64> {
        let target = |id: u64| match self.change(id).map(|c| &c.action) {
            Some(Action::Undo { change }) => Some(*change),
            _ => None,
        };
        let mut undone = HashSet::new();
        for c in &self.changes {
            if let Action::Undo { change } = c.action {
                undone.insert(change);
                // Undoing an undo brings back what it undid.
                if let Some(redone) = target(change) {
                    undone.remove(&redone);
                }
            }
        }
        undone
    }

    /// Change `id`.
    pub fn change(&self, id: u64) -> Option<&Change> {
        self.changes.binary_search_by_key(&id, |c| c.id).ok().map(|i| &self.changes[i])
    }

    /// Takes old undos out of a log from before undo moved forward ([`Action::Revert`],
    /// [`Action::Rollback`] and [`Action::Restore`] changed which changes count), with
    /// what they undid, so the world is the same. Returns whether anything changed.
    pub fn migrate(&mut self) -> bool {
        if !self.changes.iter().any(|c| c.action.is_legacy()) {
            return false;
        }
        let may_revert = |player: &str, id: u64| self.is_admin(player) || self.change(id).is_some_and(|c| c.by.player == player);
        // Newest first, so undoing an undo brought its changes back.
        let mut undone = HashSet::new();
        for (i, c) in self.changes.iter().enumerate().rev() {
            if undone.contains(&c.id) {
                continue;
            }
            match &c.action {
                Action::Revert { change } if may_revert(&c.by.player, *change) => {
                    undone.insert(*change);
                }
                Action::Rollback { changes, .. } if self.is_admin(&c.by.player) => undone.extend(changes),
                Action::Restore { to } if self.is_admin(&c.by.player) => {
                    undone.extend(self.changes[..i].iter().rev().take_while(|c| c.id > *to).map(|c| c.id));
                }
                _ => {}
            }
        }
        self.changes.retain(|c| !undone.contains(&c.id) && !c.action.is_legacy());
        true
    }

    /// Every save, newest last: its change and name.
    pub fn saves(&self) -> impl DoubleEndedIterator<Item = (&Change, &str)> {
        self.changes.iter().filter_map(|c| match &c.action {
            Action::Save { name } => Some((c, name.as_str())),
            _ => None,
        })
    }

    /// The newest save named `name`.
    pub fn save_named(&self, name: &str) -> Option<&Change> {
        self.saves().rev().find(|(_, n)| n.eq_ignore_ascii_case(name)).map(|(c, _)| c)
    }

    /// The log up to change `upto`.
    pub fn upto(&self, upto: u64) -> Self {
        Self { changes: self.changes.iter().take_while(|c| c.id <= upto).cloned().collect(), ..self.clone() }
    }

    /// A new world that starts as this one was at its save `save` (named `name`), whose
    /// state is `state`: the same seed, rules, admins and packages, and a first change
    /// that loads it (by the world, so it needs no admin).
    pub fn fork(&self, save: u64, name: &str, state: WorldState, at: u64) -> Self {
        let by = Provenance::new(WORLD, WORLD, None, None, at);
        let load = Action::Load { save, name: name.into(), state: Some(Box::new(state)) };
        Self { changes: vec![Change { id: 1, by, action: load, sig: None }], ..self.clone() }
    }

    /// A stable fingerprint of the whole log, for checking a cache is current.
    pub fn hash(&self) -> u64 {
        fnv1a(serde_json::to_string(self).unwrap_or_default().as_bytes())
    }
}

/// 64-bit FNV-1a: tiny, and stable across Rust versions (unlike `DefaultHasher`).
pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ *b as u64).wrapping_mul(0x0000_0100_0000_01b3))
}

/// The world a log describes, as of some change.
#[derive(Clone, Default)]
pub struct Replay {
    pub seed: u32,
    pub flat: bool,
    pub admins: Vec<String>,
    pub builds: BTreeMap<u64, BuildState>,
    /// Voxel edits on top of the seed.
    pub edits: HashMap<IVec3, VoxelId>,
    pub land: Land,
    /// The rules in force after the last change.
    pub rules: WorldRules,
    pub rates: Rates,
    pub budgets: Budgets,
    /// What each placed build costs, and whom it's charged to.
    pub footprints: HashMap<u64, Footprint>,
    /// Game packages, entities, and what's installed where.
    pub logic: Logic,
    /// Builds that are portals, and where they lead.
    pub portals: BTreeMap<u64, PortalTo>,
    /// Changes that no longer apply (refused by the land rules, or by tightened
    /// limits), with why.
    pub problems: Vec<String>,
    /// What the latest changes did, by change, so they can be undone ([`crate::undo`]).
    pub undoable: BTreeMap<u64, UndoRecord>,
    /// What the last change's game did to the world's voxels, and who installed it:
    /// for a game that applies changes without voxels to draw it.
    pub last_world: Option<(String, Vec<WorldChange>)>,
    /// The voxels the last change (an undo) set back, for whoever draws them.
    pub last_cells: Vec<Cell>,
    /// Whether the last change loaded a save: everything may differ.
    pub reloaded: bool,
}

pub fn parcels_of(build: &BuildState) -> Vec<IVec2> {
    build_parcels(Vec3::from_array(build.position), build.yaw, &build.spec.size)
}

/// What a terrain change costs: its changed voxels, charged to its author.
pub fn terrain_footprint(player: &str, changed: usize) -> Footprint {
    Footprint { owner: player.into(), parcels: Vec::new(), usage: Usage { terrain: changed, ..Default::default() } }
}

/// Rebuilds everything: builds, land, budgets, and voxel edits.
pub fn replay(log: &WorldLog) -> Replay {
    walk(log, true, None)
}

/// Like [`replay`], with `runtime` answering intents, so the game's entities come back
/// too.
pub fn replay_with(log: &WorldLog, runtime: Arc<dyn Runtime>) -> Replay {
    walk(log, true, Some(runtime))
}

/// Builds and land only, skipping the voxel work: cheap enough to redo after every change.
pub fn replay_land(log: &WorldLog) -> Replay {
    walk(log, false, None)
}

/// The whole world as of some change: what a [`Snapshot`] keeps and a load swaps in.
/// Rate limits aren't in it: they're about the last few minutes, not the world.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WorldState {
    pub rules: WorldRules,
    #[serde(default)]
    pub builds: Vec<Placed>,
    #[serde(default)]
    pub parcels: Vec<SavedParcel>,
    /// Terrain voxels charged to each player.
    #[serde(default)]
    pub terrain: Vec<(String, usize)>,
    #[serde(default)]
    pub installed: Vec<Install>,
    /// The games' entities, inventories, stats and timers.
    #[serde(default)]
    pub game: GameState,
    #[serde(default)]
    pub timers_until: u64,
    /// Every voxel edit, as `[x, y, z, block]`, sorted.
    #[serde(default)]
    pub edits: Vec<[i32; 4]>,
    /// What the latest changes did, to undo them.
    #[serde(default)]
    pub undoable: Vec<(u64, UndoRecord)>,
}

/// A claimed parcel, in a [`WorldState`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedParcel {
    pub at: [i32; 2],
    pub owner: String,
    pub owner_name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub builders: Vec<String>,
}

/// The world as of change `upto`, so loading it (a restart, a player joining) only
/// applies the changes after it. It stays valid for as long as the log does: the log
/// only grows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The last change it includes.
    pub upto: u64,
    /// That change's fingerprint, so a snapshot of another log with the same ids (a
    /// world started over) doesn't fit.
    pub at: u64,
    pub state: WorldState,
}

/// A change's fingerprint: what its author signed.
fn print_of(change: &Change) -> u64 {
    fnv1a(&signing_bytes(&change.by, &change.action))
}

impl Snapshot {
    /// The world `state` (replayed with voxels) as of the log's last change.
    pub fn take(log: &WorldLog, state: &Replay) -> Self {
        Self::new(log, state.state())
    }

    /// `state`, the world as of the log's last change.
    pub fn new(log: &WorldLog, state: WorldState) -> Self {
        Self::at(log, log.changes.last().map_or(0, |c| c.id), state)
    }

    /// `state`, the world as of the log's change `upto`.
    pub fn at(log: &WorldLog, upto: u64, state: WorldState) -> Self {
        Self { upto, at: log.change(upto).map_or(0, print_of), state }
    }

    /// Whether it's a snapshot of this log.
    pub fn fits(&self, log: &WorldLog) -> bool {
        self.upto == 0 || log.change(self.upto).is_some_and(|c| print_of(c) == self.at)
    }
}

/// Like [`replay`], but starting from `snapshot` when it fits the log: only the changes
/// after it are applied.
pub fn replay_from(log: &WorldLog, snapshot: Option<&Snapshot>) -> Replay {
    let Some(snapshot) = snapshot.filter(|s| s.fits(log)) else {
        return replay(log);
    };
    let mut out = Replay::new(log);
    out.load_state(&snapshot.state, true);
    for change in log.changes.iter().filter(|c| c.id > snapshot.upto) {
        if let Err(why) = out.apply(change, true) {
            out.problems.push(format!("change {} by {}: {why}", change.id, change.by.player_name));
        }
    }
    out
}

/// The world as of change `upto` (a save), from `snapshot` if it's from before then.
pub fn state_at(log: &WorldLog, upto: u64, snapshot: Option<&Snapshot>) -> WorldState {
    replay_from(&log.upto(upto), snapshot.filter(|s| s.upto <= upto)).state()
}

/// Edge length, in voxels, of the chunks fingerprints cover (the game's mesh chunks).
pub const PRINT_CHUNK: i32 = 32;

pub fn chunk_of(p: IVec3) -> IVec3 {
    p.div_euclid(IVec3::splat(PRINT_CHUNK))
}

/// The edits inside each of `chunks`, as `[x, y, z, block]`, sorted.
pub fn chunk_edits(edits: &HashMap<IVec3, VoxelId>, chunks: &[IVec3]) -> HashMap<IVec3, Vec<[i32; 4]>> {
    let mut out: HashMap<IVec3, Vec<[i32; 4]>> = chunks.iter().map(|c| (*c, Vec::new())).collect();
    for (p, id) in edits {
        if let Some(list) = out.get_mut(&chunk_of(*p)) {
            list.push([p.x, p.y, p.z, *id as i32]);
        }
    }
    for list in out.values_mut() {
        list.sort_unstable();
    }
    out
}

/// A fingerprint of each of `chunks`' edits: two worlds whose fingerprints match have
/// the same voxels there.
pub fn chunk_prints(edits: &HashMap<IVec3, VoxelId>, chunks: &[IVec3]) -> Vec<([i32; 3], u64)> {
    let mut prints: Vec<([i32; 3], u64)> = chunk_edits(edits, chunks)
        .into_iter()
        .map(|(c, list)| {
            let bytes: Vec<u8> = list.iter().flatten().flat_map(|v| v.to_le_bytes()).collect();
            (c.to_array(), fnv1a(&bytes))
        })
        .collect();
    prints.sort_unstable();
    prints
}

fn walk(log: &WorldLog, voxels: bool, runtime: Option<Arc<dyn Runtime>>) -> Replay {
    let mut out = Replay::new(log);
    out.logic.runtime = runtime;
    for change in &log.changes {
        if let Err(why) = out.apply(change, voxels) {
            out.problems.push(format!("change {} by {}: {why}", change.id, change.by.player_name));
        }
    }
    out
}

impl Replay {
    /// Applies what a game did to the world's voxels, as `installer` (whose land and
    /// budget it uses): all of it, or none of it if any part isn't allowed. Returns the
    /// voxel edits, if `voxels`.
    fn apply_world(&mut self, installer: &str, world: &[WorldChange], voxels: bool) -> Result<Vec<(IVec3, VoxelId)>, String> {
        if world.is_empty() {
            return Ok(Vec::new());
        }
        // Everything is checked against copies first; the world changes only after.
        let mut budgets = self.budgets.clone();
        let mut placed = Vec::new();
        let mut removed = Vec::new();
        let mut made = Vec::new();
        for change in world {
            match change {
                WorldChange::Build { id, package, model, spec, position, yaw } => {
                    let build = BuildState {
                        build: *id,
                        name: model.clone(),
                        prompt: String::new(),
                        provider: package.clone(),
                        spec: spec.clone(),
                        position: *position,
                        yaw: *yaw,
                    };
                    let claims = self.land.authorize(installer, &parcels_of(&build))?;
                    if !claims.is_empty() {
                        return Err(format!("{package} builds only on land that whoever installed it has"));
                    }
                    let footprint = Footprint { owner: installer.into(), parcels: parcels_of(&build), usage: build_usage(spec) };
                    budgets.check(&footprint, None, &self.rules.budgets).map_err(|e| format!("{package}: {e}"))?;
                    budgets.add(&footprint);
                    placed.push((build, footprint));
                }
                WorldChange::Unbuild { id } => removed.push(*id),
                WorldChange::Terrain { package, area, spec } => {
                    let claims = self.land.authorize(installer, &area_parcels(*area))?;
                    if !claims.is_empty() {
                        return Err(format!("{package} reshapes only land that whoever installed it has"));
                    }
                    // What it costs is its area, so the cost is the same with or without voxels.
                    let cost = terrain_footprint(installer, (area.size[0] * area.size[1]).max(0) as usize);
                    budgets.check(&cost, None, &self.rules.budgets).map_err(|e| format!("{package}: {e}"))?;
                    budgets.add(&cost);
                    if voxels {
                        let terrain = Terrain { seed: self.seed, flat: self.flat };
                        let columns = ground_columns(terrain, *area, &self.edits);
                        let edit = sculpt::compile(spec, *area, terrain, columns, &self.rules.terrain)
                            .map_err(|errors| format!("{package}'s land change: {}", errors.join("; ")))?;
                        let (_, edits) = self.land.authorize_terrain(installer, *area, edit.edits).expect("authorized above");
                        made.extend(edits);
                    }
                }
            }
        }
        self.budgets = budgets;
        for (build, footprint) in placed {
            self.footprints.insert(build.build, footprint);
            self.builds.insert(build.build, build);
        }
        for id in removed {
            if let Some(old) = self.footprints.remove(&id) {
                self.budgets.remove(&old);
            }
            self.builds.remove(&id);
            self.portals.remove(&id);
        }
        self.edits.extend(made.iter().copied());
        Ok(made)
    }

    /// The whole world as it is now (with its voxels, if it was replayed with them).
    pub fn state(&self) -> WorldState {
        let mut edits: Vec<[i32; 4]> = self.edits.iter().map(|(p, id)| [p.x, p.y, p.z, *id as i32]).collect();
        edits.sort_unstable();
        let mut parcels: Vec<SavedParcel> = self
            .land
            .parcels
            .iter()
            .map(|(at, p)| SavedParcel { at: at.to_array(), owner: p.owner.clone(), owner_name: p.owner_name.clone(), builders: p.builders.clone() })
            .collect();
        parcels.sort_by_key(|p| p.at);
        let mut terrain: Vec<(String, usize)> = self.budgets.players.iter().filter(|(_, u)| u.terrain > 0).map(|(p, u)| (p.clone(), u.terrain)).collect();
        terrain.sort();
        WorldState {
            rules: self.rules.clone(),
            builds: self.builds.keys().filter_map(|id| self.placed(*id)).collect(),
            parcels,
            terrain,
            installed: self.logic.installed.values().cloned().collect(),
            game: self.logic.state.clone(),
            timers_until: self.logic.timers_until,
            edits,
            undoable: self.undoable.iter().map(|(id, r)| (*id, r.clone())).collect(),
        }
    }

    /// Becomes `state`, keeping the seed, admins, packages and rate limits; its voxels
    /// too if `voxels`.
    pub fn load_state(&mut self, state: &WorldState, voxels: bool) {
        self.rules = state.rules.clone();
        self.land = Land::new(self.admins.clone(), self.rules.spawn.clone(), self.rules.land);
        for p in &state.parcels {
            let parcel = Parcel { owner: p.owner.clone(), owner_name: p.owner_name.clone(), builders: p.builders.clone() };
            self.land.parcels.insert(IVec2::from_array(p.at), parcel);
        }
        self.builds.clear();
        self.footprints.clear();
        self.portals.clear();
        self.budgets = Budgets::default();
        for placed in &state.builds {
            let id = placed.build.build;
            let footprint = Footprint { owner: placed.owner.clone(), parcels: parcels_of(&placed.build), usage: build_usage(&placed.build.spec) };
            self.budgets.add(&footprint);
            self.footprints.insert(id, footprint);
            self.builds.insert(id, placed.build.clone());
            if let Some(portal) = &placed.portal {
                self.portals.insert(id, portal.clone());
            }
        }
        for (player, voxels) in &state.terrain {
            self.budgets.add(&terrain_footprint(player, *voxels));
        }
        self.logic.installed = state.installed.iter().map(|i| (i.package.name.clone(), i.clone())).collect();
        self.logic.state = state.game.clone();
        self.logic.timers_until = state.timers_until;
        self.edits = if voxels { state.edits.iter().map(|[x, y, z, id]| (IVec3::new(*x, *y, *z), *id as VoxelId)).collect() } else { HashMap::new() };
        self.undoable = state.undoable.iter().cloned().collect();
    }

    /// The world before any change: the log's seed, admins and starting rules.
    pub fn new(log: &WorldLog) -> Self {
        let rules = log.rules.clone();
        let land = Land::new(log.admins.clone(), rules.spawn.clone(), rules.land);
        let logic = Logic { packages: log.packages.clone(), ..Default::default() };
        Self { seed: log.seed, flat: log.flat, admins: log.admins.clone(), land, rules, logic, ..Default::default() }
    }

    /// Applies one change on top of the current state, or says why the rules refuse it
    /// (leaving the state as it was). Returns the voxel edits it made, if `voxels` (an
    /// undo returns the voxels it sets back either way: it knows them).
    pub fn apply(&mut self, change: &Change, voxels: bool) -> Result<Vec<(IVec3, VoxelId)>, String> {
        self.last_world = None;
        self.last_cells.clear();
        self.reloaded = false;
        let by = &change.by;
        let parcels = |list: &[[i32; 2]]| list.iter().map(|p| IVec2::from_array(*p)).collect::<Vec<_>>();
        // The server's own changes (timers, players carrying things between worlds)
        // count as an admin's: no player can sign as the world.
        let admin = self.admins.contains(&by.player) || by.player == WORLD;
        // Rate limits count changes that add to the world, by their voxels.
        let cost = match &change.action {
            Action::PlaceBuild(b) | Action::UpdateBuild(b) => Some(build_usage(&b.spec).voxels),
            Action::Terrain { changed, .. } => Some(*changed),
            Action::RemoveBuild { .. }
            | Action::ClaimParcels { .. }
            | Action::InstallPackage { .. }
            | Action::UninstallPackage { .. }
            | Action::Save { .. }
            | Action::Undo { .. }
            | Action::SetPortal { .. } => Some(0),
            _ => None,
        };
        if let Some(cost) = cost.filter(|_| !admin) {
            self.rates.check(&self.rules.rates, &by.player, by.at, cost)?;
        }
        // A game that is the world keeps players playing, not building.
        let builds = matches!(
            change.action,
            Action::PlaceBuild(_) | Action::UpdateBuild(_) | Action::RemoveBuild { .. } | Action::Terrain { .. } | Action::ClaimParcels { .. }
        );
        if builds && !admin && !self.logic.building_allowed() {
            return Err("this world is a game right now: only admins build here".into());
        }
        let mut made = Vec::new();
        match &change.action {
            Action::PlaceBuild(b) if self.builds.contains_key(&b.build) => {
                return Err(format!("build id {} is taken", b.build));
            }
            Action::PlaceBuild(b) | Action::UpdateBuild(b) => {
                let before = self.placed(b.build);
                // Moving a build needs rights where it was and where it goes.
                let mut touched = parcels_of(b);
                touched.extend(self.builds.get(&b.build).map(parcels_of).unwrap_or_default());
                let old = self.footprints.get(&b.build);
                let new = Footprint {
                    owner: old.map_or(by.player.clone(), |o| o.owner.clone()),
                    parcels: parcels_of(b),
                    usage: build_usage(&b.spec),
                };
                let claims = self.land.authorize(&by.player, &touched)?;
                self.budgets.check(&new, old, &self.rules.budgets)?;
                self.land.claim(&by.player, &by.player_name, &claims);
                if let Some(old) = self.footprints.remove(&b.build) {
                    self.budgets.remove(&old);
                }
                self.budgets.add(&new);
                self.footprints.insert(b.build, new);
                self.builds.insert(b.build, b.clone());
                let op = Op::Build { id: b.build, from: before.map(Box::new), to: self.placed(b.build).map(Box::new) };
                self.remember(change.id, &by.player, false, vec![op]);
            }
            Action::RemoveBuild { build } => {
                let touched = self.builds.get(build).map(parcels_of).unwrap_or_default();
                self.land.authorize(&by.player, &touched)?;
                if let Some(before) = self.placed(*build) {
                    let op = Op::Build { id: *build, from: Some(Box::new(before)), to: None };
                    self.remember(change.id, &by.player, false, vec![op]);
                }
                self.builds.remove(build);
                self.portals.remove(build);
                self.logic.state.forget_build(*build);
                if let Some(old) = self.footprints.remove(build) {
                    self.budgets.remove(&old);
                }
            }
            Action::Terrain { name, area, spec, changed } => {
                let cost = terrain_footprint(&by.player, *changed);
                let claims = self.land.authorize(&by.player, &area_parcels(*area))?;
                self.budgets.check(&cost, None, &self.rules.budgets)?;
                let mut cells = Vec::new();
                if voxels {
                    let terrain = Terrain { seed: self.seed, flat: self.flat };
                    let columns = ground_columns(terrain, *area, &self.edits);
                    let ground = columns.clone();
                    let edit = sculpt::compile(spec, *area, terrain, columns, &self.rules.terrain)
                        .map_err(|errors| format!("\"{name}\" {}", errors.join("; ")))?;
                    let (_, edits) =
                        self.land.authorize_terrain(&by.player, *area, edit.edits).expect("authorized above");
                    cells = edits
                        .iter()
                        .map(|(p, id)| Cell { p: p.to_array(), from: (self.edits.get(p).copied(), ground.original(*p).unwrap_or(*id)), to: (Some(*id), *id) })
                        .collect();
                    self.edits.extend(edits.iter().copied());
                    made = edits;
                }
                self.budgets.add(&cost);
                self.land.claim(&by.player, &by.player_name, &claims);
                let op = Op::Land { owner: by.player.clone(), area: *area, cost: *changed, charge: true, cells };
                self.remember(change.id, &by.player, false, vec![op]);
            }
            Action::ClaimParcels { parcels: list } => {
                let claims = self.land.authorize(&by.player, &parcels(list))?;
                self.land.claim(&by.player, &by.player_name, &claims);
            }
            Action::ReleaseParcels { parcels: list } => self.land.release(&by.player, &parcels(list)),
            Action::LegacyVoxels { edits } => {
                if voxels {
                    made = edits.iter().map(|[x, y, z, id]| (IVec3::new(*x, *y, *z), *id as VoxelId)).collect();
                    self.edits.extend(made.iter().copied());
                }
            }
            Action::SetRules { rules } if admin => {
                let op = Op::Rules { from: Box::new(self.rules.clone()), to: Box::new(rules.clone()) };
                self.land.mode = rules.land;
                self.land.spawn = rules.spawn.clone();
                self.rules = rules.clone();
                self.remember(change.id, &by.player, false, vec![op]);
            }
            Action::SetRules { .. } => return Err("only admins change the rules".into()),
            Action::InstallPackage { package, scope } => {
                let claims = self.land.authorize(&by.player, &scope.parcels())?;
                let from = self.logic.installed.get(&package.name).cloned();
                self.logic.install(&by.player, admin, &self.rules.logic, package, scope)?;
                // New to the world, or a new version: the game sets it up (`on_start`,
                // with `ev.upgrade` for a new version, which keeps what's there).
                let upgrade = from.as_ref().filter(|f| f.package != *package).map(|old| {
                    let old = self.logic.packages.get(&old.package.hash);
                    let new = self.logic.packages.get(&package.hash);
                    match (old, new) {
                        (Some(old), Some(new)) => crate::logic::Upgrade::between(old, new),
                        _ => crate::logic::Upgrade::default(),
                    }
                });
                if from.is_none() || upgrade.is_some() {
                    let start = self
                        .logic
                        .prepare_start(change.id, &by.player, by.at, self.seed, &self.rules.logic, &package.name, upgrade.as_ref())
                        .and_then(|prepared| {
                            let world = self.apply_world(&prepared.installer, &prepared.world, voxels)?;
                            Ok((prepared, world))
                        });
                    match start {
                        Ok((prepared, world)) => {
                            made = world;
                            self.last_world = Some((prepared.installer.clone(), prepared.world.clone()));
                            self.logic.commit(prepared);
                        }
                        Err(why) => {
                            match &from {
                                Some(old) => self.logic.installed.insert(package.name.clone(), old.clone()),
                                None => self.logic.installed.remove(&package.name),
                            };
                            return Err(format!("{}'s on_start: {why}", package.name));
                        }
                    }
                }
                self.land.claim(&by.player, &by.player_name, &claims);
                let to = self.logic.installed.get(&package.name).cloned();
                self.remember(change.id, &by.player, false, vec![Op::Install { name: package.name.clone(), from, to }]);
            }
            Action::UninstallPackage { name } => self.logic.uninstall(&by.player, admin, &self.rules.logic, name)?,
            Action::Intent { intent } => {
                let prepared = self.logic.prepare(change.id, &by.player, by.at, self.seed, &self.rules.logic, intent)?;
                made = self.apply_world(&prepared.installer, &prepared.world, voxels)?;
                self.last_world = Some((prepared.installer.clone(), prepared.world.clone()));
                self.logic.commit(prepared);
            }
            Action::Timers { .. } if !admin => return Err("only the world runs timers".into()),
            Action::Depart { .. } | Action::Arrive { .. } if !admin => return Err("only the world moves players between worlds".into()),
            Action::Depart { player, items, .. } => {
                self.logic.state.carry_out(player, items)?;
                self.logic.state.carry_stats(player, None);
            }
            Action::Arrive { player, items, stats, .. } => {
                self.logic.state.carry_in(player, items);
                self.logic.state.carry_stats(player, Some(stats));
            }
            Action::Timers { package, until, effects } => {
                let prepared = self.logic.prepare_timers(change.id, package, *until, effects, &self.rules.logic)?;
                made = self.apply_world(&prepared.installer, &prepared.world, voxels)?;
                self.last_world = Some((prepared.installer.clone(), prepared.world.clone()));
                self.logic.commit(prepared);
            }
            Action::Save { name } if name.trim().is_empty() || name.chars().count() > 40 => {
                return Err("a save's name is 1 to 40 characters".into());
            }
            Action::Save { .. } => {}
            Action::SetPortal { build, to } => {
                let placed = self.builds.get(build).ok_or(format!("there's no build {build}"))?;
                self.land.authorize(&by.player, &parcels_of(placed))?;
                let before = self.placed(*build);
                match to {
                    Some(to) => {
                        PortalTo::parse(&to.label())?;
                        self.portals.insert(*build, to.clone());
                    }
                    None => {
                        self.portals.remove(build);
                    }
                }
                let op = Op::Build { id: *build, from: before.map(Box::new), to: self.placed(*build).map(Box::new) };
                self.remember(change.id, &by.player, false, vec![op]);
            }
            Action::Undo { change: target } => made = self.undo(change.id, *target, by, admin, voxels)?,
            Action::Load { .. } if !admin => return Err("only admins load saves".into()),
            Action::Load { state: None, name, .. } => return Err(format!("the save \"{name}\" didn't come with the load")),
            Action::Load { state: Some(state), .. } => {
                self.load_state(state, voxels);
                self.reloaded = true;
            }
            Action::Revert { .. } | Action::Rollback { .. } | Action::Restore { .. } => {
                return Err("an undo from an old log (WorldLog::migrate takes those out)".into());
            }
        }
        // A change that went through counts toward its author's rate.
        if let Some(cost) = cost.filter(|_| !admin) {
            self.rates.add(&by.player, by.at, cost);
        }
        Ok(made)
    }
}

/// The world's ground for walking pieces: the seed's heights, with the voxel edits on
/// top (a replay without voxels knows only the seed's).
impl crate::logic::Ground for Replay {
    fn heights(&self, min: [i32; 2], step: i32, count: [i32; 2]) -> Option<Vec<Option<i32>>> {
        let terrain = Terrain { seed: self.seed, flat: self.flat };
        let step = step.max(1);
        let max = [min[0] + count[0] * step, min[1] + count[1] * step];
        // The edits in the sampled columns, by column.
        let mut edited: HashMap<(i32, i32), Vec<(i32, VoxelId)>> = HashMap::new();
        for (p, id) in &self.edits {
            let (dx, dz) = (p.x - min[0], p.z - min[1]);
            if p.x >= min[0] && p.z >= min[1] && p.x < max[0] && p.z < max[1] && dx % step == 0 && dz % step == 0 {
                edited.entry((p.x, p.z)).or_default().push((p.y, *id));
            }
        }
        let mut out = Vec::with_capacity((count[0] * count[1]).max(0) as usize);
        for j in 0..count[1] {
            for i in 0..count[0] {
                let (x, z) = (min[0] + i * step, min[1] + j * step);
                let h = terrain.height(x, z);
                let sea = !self.flat && h < crate::terrain::SEA_LEVEL;
                let Some(edits) = edited.get(&(x, z)) else {
                    out.push((!sea).then_some(h));
                    continue;
                };
                // An edited column: as generated, with the edits over it.
                let mut column: Vec<VoxelId> = (0..WORLD_HEIGHT)
                    .map(|y| {
                        if y <= h {
                            crate::terrain::block::STONE[0]
                        } else if !self.flat && y <= crate::terrain::SEA_LEVEL {
                            crate::terrain::block::WATER
                        } else {
                            crate::voxel::AIR
                        }
                    })
                    .collect();
                for (y, id) in edits {
                    if let Some(v) = column.get_mut(*y as usize) {
                        *v = *id;
                    }
                }
                out.push(crate::logic::walk::stand_in(&column));
            }
        }
        Some(out)
    }
}

/// The columns a terrain change in `area` reads, generated from the seed with `edits`
/// on top: what the loaded world would show, without needing it loaded.
pub fn ground_columns(terrain: Terrain, area: Area, edits: &HashMap<IVec3, VoxelId>) -> Columns {
    let (min, size) = Columns::bounds(area);
    let origin = IVec3::new(min.x, 0, min.y);
    let mut grid = terrain.generate(origin, UVec3::new(size.x as u32, WORLD_HEIGHT as u32, size.y as u32));
    let max = min + size;
    for (p, id) in edits {
        let c = IVec2::new(p.x, p.z);
        if c.cmpge(min).all() && c.cmplt(max).all() {
            grid.set(*p - origin, *id);
        }
    }
    Columns::read(area, |x, z, out| {
        for (y, v) in out.iter_mut().enumerate() {
            *v = grid.get(IVec3::new(x - min.x, y as i32, z - min.y));
        }
        true
    })
    .expect("generated columns are always available")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        logic::{Batch, EntityId, Event, Grants, LogicMode, Trigger, package::tests::farm},
        sculpt::{OpArea, TerrainOp},
    };

    fn by(player: &str) -> Provenance {
        Provenance::new(player, player, Some("Claude Code"), Some("a pond"), 0)
    }

    fn pond(x: i32) -> Action {
        let op = TerrainOp {
            kind: "water".into(),
            area: Some(OpArea { shape: "ellipse".into(), x: 1.0, z: 1.0, w: 4.0, d: 4.0 }),
            points: None,
            amount_m: None,
            level_m: None,
            depth_m: Some(1.0),
            width_m: None,
            edge_m: Some(1.0),
            material: None,
            item: None,
            density: None,
        };
        Action::Terrain {
            name: "Pond".into(),
            area: Area { min: [x, 0], size: [24, 24] },
            spec: TerrainSpec { name: "Pond".into(), ops: vec![op] },
            changed: 0,
        }
    }

    fn lamp(build: u64) -> Action {
        let spec: BuildSpec = serde_json::from_str(
            r##"{"name":"Lamp","size":[3,4,3],"palette":[{"name":"iron","hex":"#3d4248","material":"metal"}],
                "shapes":[{"kind":"box","color":"iron","from":[0,0,0],"to":[3,4,3],"radius":null,"wobble":null}]}"##,
        )
        .unwrap();
        Action::PlaceBuild(BuildState {
            build,
            name: "Lamp".into(),
            prompt: "a lamp".into(),
            provider: "Claude Code".into(),
            spec,
            position: [0.0, 10.0, 0.0],
            yaw: 0.0,
        })
    }

    #[test]
    fn only_the_world_carries_things_between_worlds() {
        let mut log = WorldLog::new(1);
        log.admins = vec!["admin".into()];
        let items = vec![("farm:seed".to_string(), 3)];
        // Bo can't make things up by arriving with them.
        log.push(by("bo"), Action::Arrive { player: "bo".into(), from: "x".into(), items: items.clone(), stats: Vec::new() });
        assert_eq!(replay_land(&log).logic.state.items("bo", "farm:seed"), 0);
        log.push(by(WORLD), Action::Arrive { player: "bo".into(), from: "x".into(), items: items.clone(), stats: Vec::new() });
        assert_eq!(replay_land(&log).logic.state.items("bo", "farm:seed"), 3);
        // Leaving takes exactly what was carried, and can't take more.
        log.push(by(WORLD), Action::Depart { player: "bo".into(), to: "y".into(), items: vec![("farm:seed".into(), 5)], stats: Vec::new() });
        assert!(replay_land(&log).problems.iter().any(|p| p.contains("doesn't carry")));
        log.push(by(WORLD), Action::Depart { player: "bo".into(), to: "y".into(), items, stats: Vec::new() });
        assert_eq!(replay_land(&log).logic.state.inventory("bo").count(), 0);
    }

    #[test]
    fn portals_link_builds_to_worlds() {
        let mut log = WorldLog::new(1);
        log.admins = vec!["admin".into()];
        let portals = |log: &WorldLog| replay_land(log).portals;
        // Away from the protected spawn area.
        let away = |mut action: Action| {
            if let Action::PlaceBuild(b) | Action::UpdateBuild(b) = &mut action {
                b.position = [100.0, 10.0, 100.0];
            }
            action
        };
        log.rules.land = crate::settings::LandMode::Parcels;
        log.push(by("ada"), away(lamp(1)));
        let to = |name: &str| Some(PortalTo::parse(name).unwrap());
        // Bo can't change Ada's build, so he can't make it a portal.
        log.push(by("bo"), Action::SetPortal { build: 1, to: to("bos-world") });
        assert!(portals(&log).is_empty());
        log.push(by("ada"), Action::SetPortal { build: 1, to: to("Sokoban") });
        assert_eq!(portals(&log)[&1], PortalTo::World { name: "sokoban".into() });
        assert_eq!(PortalTo::parse("host.example:7780"), Ok(PortalTo::Server { addr: "host.example:7780".into() }));
        assert!(PortalTo::parse("no spaces").is_err());
        // Moving the build keeps the link; removing it drops the link.
        let Action::PlaceBuild(mut moved) = away(lamp(1)) else { unreachable!() };
        moved.position[0] += 2.0;
        log.push(by("ada"), Action::UpdateBuild(moved));
        assert_eq!(portals(&log).len(), 1);
        log.push(by("ada"), Action::RemoveBuild { build: 1 });
        assert!(portals(&log).is_empty());
        log.push(by("ada"), Action::SetPortal { build: 1, to: to("sokoban") });
        assert!(replay_land(&log).problems.iter().any(|p| p.contains("no build 1")));
    }

    #[test]
    fn loading_a_save_puts_the_world_back_exactly() {
        let mut log = WorldLog::new(1);
        log.admins = vec!["admin".into()];
        let builds = |log: &WorldLog| replay_land(log).builds.keys().copied().collect::<Vec<_>>();
        log.push(by("admin"), lamp(1));
        let first = log.push(by("ada"), Action::Save { name: "first".into() });
        let at_first = state_at(&log, first, None);
        log.push(by("admin"), lamp(3));
        let second = log.push(by("ada"), Action::Save { name: "second".into() });
        let at_second = state_at(&log, second, None);
        log.push(by("admin"), lamp(4));
        assert_eq!(builds(&log), vec![1, 3, 4]);
        let load = |save: u64, name: &str, state: &WorldState| Action::Load { save, name: name.into(), state: Some(Box::new(state.clone())) };
        // Players save, but only admins load.
        log.push(by("ada"), load(first, "first", &at_first));
        assert_eq!(builds(&log), vec![1, 3, 4]);
        log.push(by("admin"), load(first, "first", &at_first));
        assert_eq!(builds(&log), vec![1]);
        // Saves made after the loaded one are still there to load, and the history stays.
        assert_eq!(log.saves().map(|(_, n)| n).collect::<Vec<_>>(), vec!["first", "second"]);
        assert_eq!(log.save_named("SECOND").map(|c| c.id), Some(second));
        log.push(by("admin"), load(second, "second", &at_second));
        assert_eq!(builds(&log), vec![1, 3]);
        // What comes after a load lands on the loaded world, and can be undone.
        let five = log.push(by("admin"), lamp(5));
        log.push(by("admin"), Action::Undo { change: five });
        assert_eq!(builds(&log), vec![1, 3]);
        // A load without the state (only its signer's part) doesn't apply.
        log.push(by("admin"), Action::Load { save: first, name: "first".into(), state: None });
        assert!(replay_land(&log).problems.last().unwrap().contains("didn't come"));
        // A fork starts from the save, with none of the history.
        let fork = log.fork(first, "first", at_first, 0);
        assert_eq!(fork.changes.len(), 1);
        assert_eq!(builds(&fork), vec![1]);
    }

    #[test]
    fn old_undos_migrate_to_the_same_world() {
        let mut log = WorldLog::new(1);
        log.admins = vec!["admin".into()];
        log.push(by("admin"), lamp(1));
        let two = log.push(by("admin"), lamp(2));
        log.push(by("admin"), lamp(3));
        log.push(by("admin"), Action::Revert { change: two });
        let save = log.push(by("admin"), Action::Save { name: "s".into() });
        log.push(by("admin"), lamp(4));
        log.push(by("bo"), Action::Rollback { changes: vec![1], note: "not an admin".into() });
        log.push(by("admin"), Action::Restore { to: save });
        assert!(log.migrate());
        assert!(!log.migrate());
        assert!(!log.changes.iter().any(|c| c.action.is_legacy()));
        let state = replay_land(&log);
        assert!(state.problems.is_empty(), "{:?}", state.problems);
        assert_eq!(state.builds.keys().copied().collect::<Vec<_>>(), vec![1, 3]);
    }

    #[test]
    fn replay_is_deterministic_and_survives_json() {
        let mut log = WorldLog::new(1337);
        // Admins aren't bound by land, which has its own test.
        log.admins = vec!["a".into(), "b".into()];
        log.push(by("a"), pond(400));
        log.push(by("a"), lamp(1));
        log.push(by("b"), pond(410));
        let first = replay(&log);
        assert!(!first.edits.is_empty() && first.problems.is_empty(), "{:?}", first.problems);
        let reloaded: WorldLog = serde_json::from_str(&serde_json::to_string(&log).unwrap()).unwrap();
        assert_eq!(reloaded.hash(), log.hash());
        let second = replay(&reloaded);
        assert_eq!(first.edits, second.edits);
        assert_eq!(first.builds, second.builds);
    }

    #[test]
    fn signed_changes_verify_and_tampering_shows() {
        let key = Keypair::from_seed([3; 32]);
        let mut log = WorldLog::new(1337);
        let me = Provenance::new(&key.player_id(), "Ada", Some("Claude Code"), Some("a lamp"), 5);
        log.push_signed(me, lamp(1), &key);
        let reloaded: WorldLog = serde_json::from_str(&serde_json::to_string(&log).unwrap()).unwrap();
        assert!(reloaded.changes[0].verified());
        // The request text isn't signed (it stays local), but everything else is.
        let mut tampered = reloaded.clone();
        tampered.changes[0].by.prompt = None;
        assert!(tampered.changes[0].verified());
        tampered.changes[0].by.agent = None;
        assert!(!tampered.changes[0].verified());
        log.push(by("b"), lamp(2));
        assert!(!log.changes[1].verified());
    }

    #[test]
    fn snapshots_skip_work_but_not_correctness() {
        let mut log = WorldLog::new(1337);
        log.admins = vec!["a".into()];
        log.push(by("a"), pond(400));
        log.push(by("a"), lamp(1));
        let snap = Snapshot::take(&log, &replay(&log));
        log.push(by("a"), pond(430));
        let full = replay(&log);
        let fast = replay_from(&log, Some(&snap));
        assert_eq!(full.edits, fast.edits);
        assert_eq!(full.builds, fast.builds);

        // Undoing a change the snapshot includes is a change after it: it still fits.
        log.push(by("a"), Action::Undo { change: 1 });
        assert!(snap.fits(&log));
        assert_eq!(replay(&log).edits, replay_from(&log, Some(&snap)).edits);
        // A snapshot of another log with the same ids doesn't.
        let mut other = log.clone();
        other.changes[1].by.player_name = "someone else".into();
        assert!(!snap.fits(&other));
        // It survives JSON, games' builds, land and all.
        let reloaded: Snapshot = serde_json::from_str(&serde_json::to_string(&snap).unwrap()).unwrap();
        assert_eq!(reloaded, snap);
    }

    #[test]
    fn undo_moves_forward_and_redoes() {
        let mut log = WorldLog::new(1337);
        log.admins = vec!["a".into(), "b".into()];
        let ids = |state: &Replay| state.builds.keys().copied().collect::<Vec<_>>();
        let placed = log.push(by("a"), lamp(1));
        log.push(by("b"), lamp(2));
        let undo = log.push(by("a"), Action::Undo { change: placed });
        assert_eq!(ids(&replay(&log)), vec![2]);
        assert_eq!(log.undone(), HashSet::from([placed]));
        // Undoing it again does nothing: it's undone already.
        log.push(by("a"), Action::Undo { change: placed });
        assert!(replay(&log).problems.last().unwrap().contains("can't be undone"));
        // Undoing the undo redoes it.
        log.push(by("a"), Action::Undo { change: undo });
        assert_eq!(ids(&replay(&log)), vec![1, 2]);
        assert!(log.undone().contains(&undo) && !log.undone().contains(&placed));

        // A change something has changed since can't be undone; the later one can first.
        let three = log.push(by("a"), lamp_at(3, 100.0));
        let Action::PlaceBuild(mut moved) = lamp_at(3, 104.0) else { unreachable!() };
        moved.name = "Moved lamp".into();
        let moved = log.push(by("a"), Action::UpdateBuild(moved));
        log.push(by("a"), Action::Undo { change: three });
        assert!(replay(&log).problems.last().unwrap().contains("changed since"));
        log.push(by("a"), Action::Undo { change: moved });
        assert_eq!(replay(&log).builds[&3].name, "Lamp");
        log.push(by("a"), Action::Undo { change: three });
        assert_eq!(ids(&replay(&log)), vec![1, 2]);

        // Land: the ground and the budget go back, and come back on redo.
        let before = replay(&log);
        let mut dig = pond(400);
        if let Action::Terrain { changed, .. } = &mut dig {
            *changed = 50;
        }
        let pond = log.push(by("a"), dig);
        let dug = replay(&log);
        assert!(dug.budgets.player("a").terrain > 0 && dug.edits.len() > before.edits.len());
        let undo = log.push(by("a"), Action::Undo { change: pond });
        let back = replay(&log);
        assert_eq!(back.budgets.player("a").terrain, 0);
        let differ = back.edits.iter().filter(|(p, id)| before.edits.get(p) != Some(id)).count();
        assert_eq!(differ, 0);
        log.push(by("a"), Action::Undo { change: undo });
        assert_eq!(replay(&log).edits, dug.edits);
        // From a snapshot (no rasterizing before it), the same.
        let snap = Snapshot::take(&log.upto(pond), &replay(&log.upto(pond)));
        assert_eq!(replay_from(&log, Some(&snap)).edits, dug.edits);
    }

    #[test]
    fn replay_enforces_land() {
        let mut log = WorldLog::new(1337);
        log.rules.land = crate::settings::LandMode::Parcels;
        log.push(by("a"), lamp(1));
        // Lamp 1 stands at the origin, in the spawn: only admins may, so it's refused...
        assert_eq!(replay_land(&log).problems.len(), 1);
        // ...until a is an admin.
        log.admins.push("a".into());
        let land = replay_land(&log);
        assert!(land.problems.is_empty() && land.builds.len() == 1);

        // b claims land by building on it; c can't build there, a (admin) can.
        let mut away = lamp(2);
        if let Action::PlaceBuild(b) = &mut away {
            b.position = [100.0, 10.0, 100.0];
        }
        log.push(by("b"), away.clone());
        let mut c = away.clone();
        if let Action::PlaceBuild(b) = &mut c {
            b.build = 3;
        }
        log.push(by("c"), c);
        let land = replay_land(&log);
        assert_eq!(land.builds.keys().copied().collect::<Vec<_>>(), vec![1, 2]);
        assert!(land.problems[0].contains("belongs to b"), "{:?}", land.problems);
        assert!(land.land.parcels.values().all(|p| p.owner == "b"));

        // An admin opens the world: from then on anyone builds anywhere but the spawn.
        let rules = WorldRules { land: crate::settings::LandMode::Open, ..log.rules.clone() };
        log.push(by("a"), Action::SetRules { rules });
        let mut d = away.clone();
        if let Action::PlaceBuild(b) = &mut d {
            b.build = 4;
        }
        log.push(by("d"), d);
        assert!(replay_land(&log).builds.contains_key(&4));
        // Only admins change rules.
        log.push(by("d"), Action::SetRules { rules: WorldRules::default() });
        assert!(replay_land(&log).problems.last().unwrap().contains("only admins"));
    }

    fn at(player: &str, t: u64) -> Provenance {
        Provenance::new(player, player, None, None, t)
    }

    fn lamp_at(build: u64, x: f32) -> Action {
        let mut a = lamp(build);
        if let Action::PlaceBuild(b) = &mut a {
            b.position = [x, 10.0, 100.0];
        }
        a
    }

    #[test]
    fn undo_is_own_only() {
        let mut log = WorldLog::new(1337);
        log.admins.push("admin".into());
        let a = log.push(at("a", 0), lamp_at(1, 100.0));
        let b = log.push(at("b", 0), lamp_at(2, 120.0));
        // b can't undo a's change; a can; an admin can undo anyone's.
        log.push(at("b", 1), Action::Undo { change: a });
        assert_eq!(replay_land(&log).builds.len(), 2);
        log.push(at("a", 2), Action::Undo { change: a });
        log.push(at("admin", 3), Action::Undo { change: b });
        assert!(replay_land(&log).builds.is_empty());
    }

    #[test]
    fn rate_limits_come_from_the_rules() {
        let mut log = WorldLog::new(1337);
        log.rules.rates.changes_per_minute = Some(2);
        for (i, t) in [0, 10, 20, 90].into_iter().enumerate() {
            log.push(at("a", t), lamp_at(i as u64 + 1, 100.0 + 10.0 * i as f32));
        }
        let state = replay_land(&log);
        // The third change in a minute is refused; a minute later it's fine again.
        assert_eq!(state.builds.keys().copied().collect::<Vec<_>>(), vec![1, 2, 4]);
        assert!(state.problems[0].contains("2 changes a minute"), "{:?}", state.problems);
    }

    #[test]
    fn generated_columns_match_however_the_region_is_cut() {
        // Chunks generate in pieces; replay generates whole areas. They must agree.
        let terrain = Terrain { seed: 1337, flat: false };
        let a = terrain.generate(IVec3::new(390, 0, -12), UVec3::new(48, WORLD_HEIGHT as u32, 48));
        let b = terrain.generate(IVec3::new(400, 0, 0), UVec3::new(32, WORLD_HEIGHT as u32, 32));
        for z in 0..32 {
            for y in 0..WORLD_HEIGHT {
                for x in 0..32 {
                    let p = IVec3::new(400 + x, y, z);
                    assert_eq!(a.get(p - IVec3::new(390, 0, -12)), b.get(p - IVec3::new(400, 0, 0)), "at {p}");
                }
            }
        }
    }

    /// Plants `farm:seed`s as crops dated by the world clock, and waters crops when
    /// they're interacted with: rules a package would carry, written by hand.
    struct Farmer;

    impl Runtime for Farmer {
        fn handle(&self, package: &Package, event: &Event, world: &mut Batch, grants: &Grants) -> Result<(), String> {
            assert_eq!(package.manifest.name, "farm");
            let Trigger::Intent(intent) = event.trigger else { return Ok(()) };
            let effect = match intent {
                Intent::Place { item, position } if item == "farm:seed" => Effect::Spawn {
                    position: *position,
                    components: [("farm:crop".to_string(), serde_json::json!({ "planted": event.time.day }))].into(),
                },
                Intent::Interact { entity } if world.get(*entity).is_some() => {
                    Effect::Set { entity: *entity, component: "farm:watered".into(), value: true.into() }
                }
                _ => return Ok(()),
            };
            world.apply(grants, &effect).map(|_| ())
        }
    }

    fn install(log: &mut WorldLog, scope: Scope) -> Action {
        let hash = log.add_package(farm(1));
        Action::InstallPackage { package: PackageRef { name: "farm".into(), version: 1, hash }, scope }
    }

    fn here() -> Scope {
        Scope::Parcels { parcels: vec![[20, 20]] }
    }

    /// Inside parcel (20, 20): 8 m per parcel.
    const PLOT: [f32; 3] = [163.0, 10.0, 163.0];

    #[test]
    fn packages_install_and_answer_intents_through_replay() {
        let mut log = WorldLog::new(1337);
        log.admins = vec!["a".into()];
        log.rules.logic.clock.day_seconds = 100;
        let install = install(&mut log, here());
        log.push(at("a", 0), install);
        let seed = Action::Intent { intent: Intent::Place { item: "farm:seed".into(), position: PLOT } };
        let planted = log.push(at("a", 250), seed.clone());
        let crop = EntityId { change: planted, n: 0 };
        log.push(at("b", 300), Action::Intent { intent: Intent::Interact { entity: crop } });
        // Outside where it's installed, no package answers the seed.
        log.push(at("a", 310), Action::Intent { intent: Intent::Place { item: "farm:seed".into(), position: [0.0; 3] } });

        let state = replay_with(&log, Arc::new(Farmer));
        assert!(state.problems.is_empty(), "{:?}", state.problems);
        let e = state.logic.state.get(crop).expect("planted");
        assert_eq!(e.components["farm:crop"]["planted"], 2);
        assert_eq!(e.components["farm:watered"], true);
        assert_eq!(state.logic.state.len(), 1);

        // The log survives JSON, packages and all, and replays the same.
        let reloaded: WorldLog = serde_json::from_str(&serde_json::to_string(&log).unwrap()).unwrap();
        assert_eq!(reloaded.hash(), log.hash());
        let again = replay_with(&reloaded, Arc::new(Farmer));
        assert_eq!(again.logic.state.get(crop), Some(e));

        // The built-in data rules don't know `farm:seed`, so there nothing grows.
        assert!(replay(&log).logic.state.is_empty());

        // Undoing the install takes the game out; redoing puts it back, crop and all.
        let undo = log.push(at("a", 400), Action::Undo { change: 1 });
        let undone = replay_with(&log, Arc::new(Farmer));
        assert!(undone.logic.installed.is_empty(), "{:?}", undone.problems);
        log.push(at("a", 410), Action::Undo { change: undo });
        let redone = replay_with(&log, Arc::new(Farmer));
        assert!(redone.logic.installed.contains_key("farm") && redone.logic.state.get(crop).is_some());
    }

    #[test]
    fn installs_follow_land_and_the_logic_mode() {
        // By default only admins change the game's rules; players can play them.
        let mut log = WorldLog::new(1337);
        log.add_package(farm(1));
        log.push(at("a", 0), install(&mut log.clone(), here()));
        assert!(replay(&log).problems[0].contains("only admins"));

        // An open world lets players install on land they may change.
        let mut log = WorldLog::new(1337);
        log.rules.logic.mode = LogicMode::Open;
        log.admins = vec!["admin".into()];
        let everywhere = install(&mut log, Scope::World);
        log.push(at("a", 0), everywhere.clone());
        log.push(at("admin", 0), everywhere.clone());
        // Someone else can't replace it, but its installer could.
        let mine = install(&mut log, here());
        log.push(at("b", 1), mine.clone());
        let state = replay(&log);
        assert_eq!(state.problems.len(), 2, "{:?}", state.problems);
        assert!(state.problems[0].contains("only an admin") && state.problems[1].contains("someone else"));
        assert_eq!(state.logic.installed["farm"].scope, Scope::World);

        // Parcels mode: installing on free land claims it, like building there would.
        let mut log = WorldLog::new(1337);
        log.rules.logic.mode = LogicMode::Open;
        log.rules.land = crate::settings::LandMode::Parcels;
        log.add_package(farm(1));
        log.push(at("a", 0), mine.clone());
        log.push(at("b", 1), Action::UninstallPackage { name: "farm".into() });
        let state = replay(&log);
        assert_eq!(state.land.parcels.get(&IVec2::new(20, 20)).map(|p| p.owner.as_str()), Some("a"), "{:?}", state.problems);
        assert!(state.problems[0].contains("someone else"), "{:?}", state.problems);

        // A locked world refuses everyone, admins included, until the rules change.
        let mut log = WorldLog::new(1337);
        log.admins = vec!["admin".into()];
        log.rules.logic.mode = LogicMode::Locked;
        log.add_package(farm(1));
        log.push(at("admin", 0), mine.clone());
        let mut open = log.rules.clone();
        open.logic.mode = LogicMode::AdminOnly;
        log.push(at("admin", 1), Action::SetRules { rules: open });
        log.push(at("a", 2), mine.clone());
        log.push(at("admin", 3), mine);
        let state = replay(&log);
        assert_eq!(state.problems.len(), 2, "{:?}", state.problems);
        assert!(state.problems[0].contains("locked") && state.problems[1].contains("only admins"));
        assert_eq!(state.logic.installed["farm"].by, "admin");
    }

    #[test]
    fn packages_must_be_uploaded_and_match() {
        let mut log = WorldLog::new(1337);
        log.admins = vec!["a".into()];
        let Action::InstallPackage { mut package, scope } = install(&mut log, here()) else { unreachable!() };
        package.version = 2;
        log.push(at("a", 0), Action::InstallPackage { package: package.clone(), scope: scope.clone() });
        package.hash = "00".into();
        log.push(at("a", 0), Action::InstallPackage { package, scope });
        let problems = replay(&log).problems;
        assert!(problems[0].contains("doesn't match") && problems[1].contains("wasn't uploaded"), "{problems:?}");
        // A log with no packages serializes as it did before packages existed.
        assert!(!serde_json::to_string(&WorldLog::new(1)).unwrap().contains("packages"));
    }
}
