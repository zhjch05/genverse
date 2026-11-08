//! The world as a log of changes: the seed plus every change, in order, each with who
//! made it. Replaying the log rebuilds the world: placed builds, and the voxel edits on
//! top of the seed (terrain changes compile against the ground the earlier changes
//! left). That's what a save is, what a server would replicate, and what makes
//! rollback a matter of replaying without some changes.
//!
//! Undo never deletes history: it appends a [`Action::Revert`] of the change it undoes
//! (players revert only their own changes), and admins can [`Action::Rollback`] any
//! set of changes: a player's, or everything after some point (restoring a version).
//! Reverts can themselves be reverted.
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
    logic::{Intent, Logic, Package, PackageRef, Runtime, Scope},
    rate::Rates,
    sculpt::{self, Area, Columns, TerrainSpec},
    settings::WorldRules,
    terrain::{Terrain, WORLD_HEIGHT},
    voxel::VoxelId,
};

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
    /// Undoes an earlier change of the same player (admins may revert anyone's).
    Revert { change: u64 },
    /// An admin undoing many changes at once: a player's, or everything after a point.
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
}

impl Action {
    /// A few words for history lists: "placed \"Stone well\"".
    pub fn describe(&self) -> String {
        match self {
            Action::PlaceBuild(b) => format!("placed \"{}\"", b.name),
            Action::UpdateBuild(b) => format!("changed \"{}\"", b.name),
            Action::RemoveBuild { build } => format!("removed build {build}"),
            Action::Terrain { name, .. } => format!("reshaped land: \"{name}\""),
            Action::ClaimParcels { parcels } => format!("claimed {} parcel(s)", parcels.len()),
            Action::ReleaseParcels { parcels } => format!("released {} parcel(s)", parcels.len()),
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
/// (which stays on their machine) or the id (which the server assigns).
pub fn signing_bytes(by: &Provenance, action: &Action) -> Vec<u8> {
    let by = Provenance { prompt: None, ..by.clone() };
    serde_json::to_vec(&(by, action)).unwrap_or_default()
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

    /// Whether `player` may revert change `id`: their own, or anyone's for admins.
    pub fn may_revert(&self, player: &str, id: u64) -> bool {
        self.is_admin(player) || self.changes.iter().any(|c| c.id == id && c.by.player == player)
    }

    /// Changes undone by reverts and rollbacks. Newest first, so undoing an undo
    /// brings its changes back.
    pub fn undone(&self) -> HashSet<u64> {
        let mut undone = HashSet::new();
        for c in self.changes.iter().rev() {
            if undone.contains(&c.id) {
                continue;
            }
            match &c.action {
                Action::Revert { change } if self.may_revert(&c.by.player, *change) => {
                    undone.insert(*change);
                }
                Action::Rollback { changes, .. } if self.is_admin(&c.by.player) => undone.extend(changes),
                _ => {}
            }
        }
        undone
    }

    /// Changes that count: not undone, and not undos themselves.
    pub fn effective(&self) -> impl Iterator<Item = &Change> {
        let undone = self.undone();
        self.changes.iter().filter(move |c| {
            !undone.contains(&c.id) && !matches!(c.action, Action::Revert { .. } | Action::Rollback { .. })
        })
    }

    /// A stable fingerprint of the whole log, for checking a cache is current.
    pub fn hash(&self) -> u64 {
        fnv1a(serde_json::to_string(self).unwrap_or_default().as_bytes())
    }

    /// The same log without every change `drop` matches (and reverts of those):
    /// rollback, e.g. of one player's changes or a time range.
    pub fn without(&self, mut drop: impl FnMut(&Change) -> bool) -> Self {
        let dropped: HashSet<u64> = self.changes.iter().filter(|c| drop(c)).map(|c| c.id).collect();
        let changes = self
            .changes
            .iter()
            .filter(|c| match c.action {
                Action::Revert { change } => !dropped.contains(&c.id) && !dropped.contains(&change),
                Action::Rollback { ref changes, .. } => {
                    !dropped.contains(&c.id) && !changes.iter().any(|id| dropped.contains(id))
                }
                _ => !dropped.contains(&c.id),
            })
            .cloned()
            .collect();
        Self { changes, ..self.clone() }
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
    /// Changes that no longer apply (refused by the land rules, or by tightened
    /// limits), with why.
    pub problems: Vec<String>,
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

/// The world's voxels as of some change, so replay (on a server restart, or a player
/// joining) only redoes the changes after it. It's only valid while the changes up to
/// it that count stay the same: undoing an older change makes it stale.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The last change it includes.
    pub upto: u64,
    /// Which changes up to `upto` were in effect when it was taken.
    pub basis: u64,
    /// Every voxel edit, as `[x, y, z, block]`, sorted.
    pub edits: Vec<[i32; 4]>,
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

/// A fingerprint of which changes up to `upto` count.
fn basis(log: &WorldLog, upto: u64) -> u64 {
    let ids: Vec<String> = log.effective().filter(|c| c.id <= upto).map(|c| c.id.to_string()).collect();
    fnv1a(ids.join(",").as_bytes())
}

impl Snapshot {
    /// The world `state` (replayed with voxels) as of the log's last change.
    pub fn take(log: &WorldLog, state: &Replay) -> Self {
        let upto = log.changes.last().map_or(0, |c| c.id);
        let mut edits: Vec<[i32; 4]> = state.edits.iter().map(|(p, id)| [p.x, p.y, p.z, *id as i32]).collect();
        edits.sort_unstable();
        Self { upto, basis: basis(log, upto), edits }
    }

    /// Whether the changes it stands for still count as they did.
    pub fn fits(&self, log: &WorldLog) -> bool {
        basis(log, self.upto) == self.basis
    }
}

/// Like [`replay`], but starting from `snapshot`'s voxels when it still fits the log:
/// only the changes after it are rasterized again.
pub fn replay_from(log: &WorldLog, snapshot: Option<&Snapshot>) -> Replay {
    let Some(snapshot) = snapshot.filter(|s| s.fits(log)) else {
        return replay(log);
    };
    let mut out = Replay::new(log);
    let mut loaded = false;
    for change in log.effective() {
        let after = change.id > snapshot.upto;
        if after && !loaded {
            out.edits = snapshot.edits.iter().map(|[x, y, z, id]| (IVec3::new(*x, *y, *z), *id as VoxelId)).collect();
            loaded = true;
        }
        if let Err(why) = out.apply(change, after) {
            out.problems.push(format!("change {} by {}: {why}", change.id, change.by.player_name));
        }
    }
    if !loaded {
        out.edits = snapshot.edits.iter().map(|[x, y, z, id]| (IVec3::new(*x, *y, *z), *id as VoxelId)).collect();
    }
    out
}

fn walk(log: &WorldLog, voxels: bool, runtime: Option<Arc<dyn Runtime>>) -> Replay {
    let mut out = Replay::new(log);
    out.logic.runtime = runtime;
    for change in log.effective() {
        if let Err(why) = out.apply(change, voxels) {
            out.problems.push(format!("change {} by {}: {why}", change.id, change.by.player_name));
        }
    }
    out
}

impl Replay {
    /// The world before any change: the log's seed, admins and starting rules.
    pub fn new(log: &WorldLog) -> Self {
        let rules = log.rules.clone();
        let land = Land::new(log.admins.clone(), rules.spawn.clone(), rules.land);
        let logic = Logic { packages: log.packages.clone(), ..Default::default() };
        Self { seed: log.seed, flat: log.flat, admins: log.admins.clone(), land, rules, logic, ..Default::default() }
    }

    /// Applies one change on top of the current state, or says why the rules refuse it
    /// (leaving the state as it was). Returns the voxel edits it made, if `voxels`.
    /// Undos ([`Action::Revert`], [`Action::Rollback`]) change which changes count, so
    /// they need a full [`replay`]; here they do nothing.
    pub fn apply(&mut self, change: &Change, voxels: bool) -> Result<Vec<(IVec3, VoxelId)>, String> {
        let by = &change.by;
        let parcels = |list: &[[i32; 2]]| list.iter().map(|p| IVec2::from_array(*p)).collect::<Vec<_>>();
        let admin = self.admins.contains(&by.player);
        // Rate limits count changes that add to the world, by their voxels.
        let cost = match &change.action {
            Action::PlaceBuild(b) | Action::UpdateBuild(b) => Some(build_usage(&b.spec).voxels),
            Action::Terrain { changed, .. } => Some(*changed),
            Action::RemoveBuild { .. }
            | Action::ClaimParcels { .. }
            | Action::InstallPackage { .. }
            | Action::UninstallPackage { .. } => Some(0),
            _ => None,
        };
        if let Some(cost) = cost.filter(|_| !admin) {
            self.rates.check(&self.rules.rates, &by.player, by.at, cost)?;
        }
        let mut made = Vec::new();
        match &change.action {
            Action::PlaceBuild(b) if self.builds.contains_key(&b.build) => {
                return Err(format!("build id {} is taken", b.build));
            }
            Action::PlaceBuild(b) | Action::UpdateBuild(b) => {
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
            }
            Action::RemoveBuild { build } => {
                let touched = self.builds.get(build).map(parcels_of).unwrap_or_default();
                self.land.authorize(&by.player, &touched)?;
                self.builds.remove(build);
                if let Some(old) = self.footprints.remove(build) {
                    self.budgets.remove(&old);
                }
            }
            Action::Terrain { name, area, spec, changed } => {
                let cost = terrain_footprint(&by.player, *changed);
                let claims = self.land.authorize(&by.player, &area_parcels(*area))?;
                self.budgets.check(&cost, None, &self.rules.budgets)?;
                if voxels {
                    let terrain = Terrain { seed: self.seed, flat: self.flat };
                    let columns = ground_columns(terrain, *area, &self.edits);
                    let edit = sculpt::compile(spec, *area, terrain, columns, &self.rules.terrain)
                        .map_err(|errors| format!("\"{name}\" {}", errors.join("; ")))?;
                    let (_, edits) =
                        self.land.authorize_terrain(&by.player, *area, edit.edits).expect("authorized above");
                    self.edits.extend(edits.iter().copied());
                    made = edits;
                }
                self.budgets.add(&cost);
                self.land.claim(&by.player, &by.player_name, &claims);
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
                self.land.mode = rules.land;
                self.land.spawn = rules.spawn.clone();
                self.rules = rules.clone();
            }
            Action::SetRules { .. } => return Err("only admins change the rules".into()),
            Action::InstallPackage { package, scope } => {
                let claims = self.land.authorize(&by.player, &scope.parcels())?;
                self.logic.install(&by.player, admin, &self.rules.logic, package, scope)?;
                self.land.claim(&by.player, &by.player_name, &claims);
            }
            Action::UninstallPackage { name } => self.logic.uninstall(&by.player, admin, &self.rules.logic, name)?,
            Action::Intent { intent } => {
                self.logic.act(change.id, &by.player, by.at, self.seed, &self.rules.logic, intent)?;
            }
            Action::Revert { .. } | Action::Rollback { .. } => {}
        }
        // A change that went through counts toward its author's rate.
        if let Some(cost) = cost.filter(|_| !admin) {
            self.rates.add(&by.player, by.at, cost);
        }
        Ok(made)
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
        logic::{Batch, Effect, EntityId, Event, Grants, LogicMode, package::tests::farm},
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

        // Undoing a change the snapshot includes makes it stale; replay still agrees.
        log.push(by("a"), Action::Revert { change: 1 });
        assert!(!snap.fits(&log));
        assert_eq!(replay(&log).edits, replay_from(&log, Some(&snap)).edits);
    }

    #[test]
    fn revert_and_rollback() {
        let mut log = WorldLog::new(1337);
        log.admins = vec!["a".into(), "b".into()];
        let placed = log.push(by("a"), lamp(1));
        log.push(by("b"), lamp(2));
        log.push(by("a"), Action::Revert { change: placed });
        assert_eq!(replay(&log).builds.keys().copied().collect::<Vec<_>>(), vec![2]);

        // Rolling back player b leaves a's history (and its revert) intact.
        let without_b = log.without(|c| c.by.player == "b");
        assert!(replay(&without_b).builds.is_empty());
        // Rolling back a's revert brings its lamp back.
        let without_revert = log.without(|c| matches!(c.action, Action::Revert { .. }));
        assert_eq!(replay(&without_revert).builds.len(), 2);
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

        // Rolling b back frees the land, and c's lamp stands after all.
        let rolled = replay_land(&log.without(|ch| ch.by.player == "b"));
        assert_eq!(rolled.builds.keys().copied().collect::<Vec<_>>(), vec![1, 3]);

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
    fn undo_is_own_only_and_chains() {
        let mut log = WorldLog::new(1337);
        log.admins.push("admin".into());
        let a = log.push(at("a", 0), lamp_at(1, 100.0));
        log.push(at("b", 0), lamp_at(2, 120.0));
        // b can't undo a's change; a can, and can undo the undo.
        log.push(at("b", 1), Action::Revert { change: a });
        assert_eq!(replay_land(&log).builds.len(), 2);
        let undo = log.push(at("a", 2), Action::Revert { change: a });
        assert_eq!(replay_land(&log).builds.len(), 1);
        log.push(at("a", 3), Action::Revert { change: undo });
        assert_eq!(replay_land(&log).builds.len(), 2);
        // An admin rolls b back; a non-admin's rollback does nothing.
        log.push(at("b", 4), Action::Rollback { changes: vec![a], note: "nope".into() });
        log.push(at("admin", 5), Action::Rollback { changes: vec![2], note: "griefing".into() });
        assert_eq!(replay_land(&log).builds.keys().copied().collect::<Vec<_>>(), vec![1]);
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
            let effect = match event.intent {
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

        // Undoing the install undoes what it would have done.
        log.push(at("a", 400), Action::Revert { change: 1 });
        let undone = replay_with(&log, Arc::new(Farmer));
        assert!(undone.logic.installed.is_empty() && undone.logic.state.is_empty());
    }

    #[test]
    fn a_data_package_plays_a_farming_loop() {
        let mut log = WorldLog::new(1337);
        log.admins = vec!["a".into()];
        log.rules.logic.clock.day_seconds = 100;
        let install = install(&mut log, here());
        log.push(at("a", 0), install);
        let intent = |intent| Action::Intent { intent };
        let place = |item: &str, position| intent(Intent::Place { item: item.into(), position });
        let touch = |entity| intent(Intent::Interact { entity });
        let plot = [PLOT[0] + 1.0, PLOT[1], PLOT[2]];

        log.push(at("a", 5), place("farm:turnip_seed", plot));
        let bin = EntityId { change: log.push(at("a", 10), place("farm:seed_bin", PLOT)), n: 0 };
        log.push(at("a", 20), touch(bin));
        log.push(at("a", 30), touch(bin));
        let crop = EntityId { change: log.push(at("a", 40), place("farm:turnip_seed", plot)), n: 0 };
        log.push(at("a", 150), touch(crop));
        log.push(at("a", 450), touch(crop));
        log.push(at("b", 460), touch(bin));
        log.push(at("a", 470), touch(bin));

        let state = replay(&log);
        let problems = &state.problems;
        assert_eq!(problems.len(), 3, "{problems:#?}");
        assert!(problems[0].contains("need 1 \"farm:turnip_seed\" and have 0"));
        assert!(problems[1].contains("already taken from the seed bin"));
        assert!(problems[2].contains("aren't ripe yet (stage 2 of 3)"));
        let game = &state.logic.state;
        assert_eq!(game.items("a", "farm:turnip"), 2);
        assert_eq!(game.items("a", "farm:turnip_seed"), 4 + 5);
        assert_eq!(game.items("b", "farm:turnip_seed"), 5);
        assert!(game.get(crop).is_none() && game.get(bin).is_some());
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
    fn live_upgrades_keep_what_is_placed() {
        let mut log = WorldLog::new(1337);
        log.admins = vec!["admin".into()];
        log.rules.logic.clock.day_seconds = 100;
        let mut spec = crate::logic::data::tests::sample_farm();
        let put = |log: &mut WorldLog, spec: &crate::logic::data::GameSpec, version, t| {
            let package = spec.to_package(version);
            let reference = package.reference();
            log.add_package(package);
            log.push(at("admin", t), Action::InstallPackage { package: reference, scope: Scope::World })
        };
        put(&mut log, &spec, 1, 0);
        let intent = |intent| Action::Intent { intent };
        let bin = EntityId { change: log.push(at("p", 1), intent(Intent::Place { item: "farm:seed_bin".into(), position: PLOT })), n: 0 };
        log.push(at("p", 2), intent(Intent::Interact { entity: bin }));
        let crop = EntityId { change: log.push(at("p", 3), intent(Intent::Place { item: "farm:turnip_seed".into(), position: PLOT })), n: 0 };
        // v2 while it grows: turnips ripen in a day. The crop already planted follows.
        spec.defs[2].stage_days = Some(vec![1]);
        put(&mut log, &spec, 2, 50);
        log.push(at("p", 150), intent(Intent::Interact { entity: crop }));
        let state = replay(&log);
        assert!(state.problems.is_empty(), "{:?}", state.problems);
        assert_eq!(state.logic.state.items("p", "farm:turnip"), 2);

        // v3 drops the seed bin: the placed bin sleeps (kept, does nothing) instead of
        // breaking anything, and wakes up when a later version brings it back.
        spec.defs.retain(|d| d.name != "seed_bin");
        put(&mut log, &spec, 3, 160);
        log.push(at("p", 260), intent(Intent::Interact { entity: bin }));
        let state = replay(&log);
        assert!(state.problems.is_empty(), "{:?}", state.problems);
        assert!(state.logic.state.get(bin).is_some());
        assert_eq!(state.logic.state.items("p", "farm:turnip_seed"), 4);
        put(&mut log, &crate::logic::data::tests::sample_farm(), 4, 270);
        log.push(at("p", 280), intent(Intent::Interact { entity: bin }));
        let state = replay(&log);
        assert!(state.problems.is_empty(), "{:?}", state.problems);
        assert_eq!(state.logic.state.items("p", "farm:turnip_seed"), 4 + 5);
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
