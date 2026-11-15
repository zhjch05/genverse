//! Game logic: what the world *does*, next to what it looks like.
//!
//! Agents write [`Package`]s: definitions as data, and Luau scripts ([`script`]) for the
//! rules data can't say. A world installs them. Players act through [`Intent`]s ("put
//! this here", "interact with that", "north"), never by sending state; the package that
//! owns the item or entity answers through a [`Batch`] that checks every change against
//! what the package may do, and lands only if all of them pass. All of it goes through
//! the log, so replay rebuilds the game's state like it rebuilds the voxels. See
//! `docs/game-logic.md`.

pub mod clock;
pub mod clockwork;
pub mod data;
pub mod entity;
#[cfg(not(target_arch = "wasm32"))]
pub mod frame;
pub mod package;
pub mod present;
pub mod script;
pub mod shader;
pub mod solid;
pub mod trial;
pub mod ui;
pub mod walk;

use std::{collections::BTreeMap, sync::Arc};

use bevy_math::{IVec2, Vec2};
use serde::{Deserialize, Serialize};

pub use clock::{ClockRules, WorldTime};
pub use clockwork::{Clockwork, Fired, Run, Seen, TimerHost};
pub use data::Def;
pub use entity::{
    Batch, Body, Effect, Entity, EntityId, GameState, Grants, Launch, Panel, Stat, Timer,
};
pub use package::{Manifest, Package, PackageRef, Permission};
pub use ui::{PanelSpec, Ui};
pub use walk::{Ground, NoGround};

use crate::{land::parcel_of, log::fnv1a, terrain::VOXEL_SIZE};

/// Who may install and change packages. Playing them is always open to everyone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogicMode {
    /// Anyone, on land they may change, with the default permissions.
    Open,
    /// Admins only: the game's rules are the world's, like its other rules.
    #[default]
    AdminOnly,
    /// Nobody, until an admin changes the rules again: for a running campaign or match.
    Locked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogicRules {
    pub mode: LogicMode,
    pub clock: ClockRules,
    /// Entities one package may have.
    pub max_entities: u32,
    /// Effects one player action may cause, over all packages.
    pub max_effects: u32,
    /// The games stand still: timers wait (what comes due meanwhile is skipped, not
    /// saved up) and players' actions in games are refused. An admin's pause, for
    /// everyone in the world. Left out of the log while off, so older rules read and
    /// sign the same.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub paused: bool,
}

impl Default for LogicRules {
    fn default() -> Self {
        Self {
            mode: LogicMode::default(),
            clock: ClockRules::default(),
            max_entities: 10_000,
            max_effects: 256,
            paused: false,
        }
    }
}

impl LogicRules {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Where a package works.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Scope {
    /// Everywhere. Needs an admin.
    World,
    /// Only on these parcels (by parcel coordinates).
    Parcels { parcels: Vec<[i32; 2]> },
}

/// The most parcels one install may cover.
pub const MAX_SCOPE_PARCELS: usize = 256;

impl Scope {
    pub fn contains(&self, position: [f32; 3]) -> bool {
        match self {
            Scope::World => true,
            Scope::Parcels { parcels } => {
                let column = (Vec2::new(position[0], position[2]) / VOXEL_SIZE)
                    .floor()
                    .as_ivec2();
                parcels.contains(&parcel_of(column).to_array())
            }
        }
    }

    pub fn parcels(&self) -> Vec<IVec2> {
        match self {
            Scope::World => Vec::new(),
            Scope::Parcels { parcels } => parcels.iter().map(|p| IVec2::from_array(*p)).collect(),
        }
    }

    fn check(&self) -> Result<(), String> {
        if let Scope::Parcels { parcels } = self {
            if parcels.is_empty() || parcels.len() > MAX_SCOPE_PARCELS {
                return Err(format!(
                    "a package goes on 1 to {MAX_SCOPE_PARCELS} parcels"
                ));
            }
            let mut sorted = parcels.clone();
            sorted.sort_unstable();
            sorted.dedup();
            if sorted.len() != parcels.len() {
                return Err("the same parcel is listed twice".into());
            }
        }
        Ok(())
    }
}

/// Something a player does that the game's rules respond to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Intent {
    Interact {
        entity: EntityId,
    },
    UseItem {
        item: String,
        target: EntityId,
    },
    /// Puts an item down somewhere (plants a seed, sets up a game board): how entities
    /// begin.
    Place {
        item: String,
        position: [f32; 3],
    },
    /// A movement key from a player on their pawn in a region that takes keys (see
    /// [`entity`]). "enter" is only in old logs: walking in is `on_enter` now.
    Key {
        entity: EntityId,
        key: String,
    },
    /// A choice made in the panel the player has open (dialogue, a menu, a shop), or
    /// closing it (`None`). It goes to the package whose panel it is.
    Choose {
        #[serde(default)]
        choice: Option<String>,
    },
    /// The player attacks a piece in reach (F): its package's `on_hit`.
    Hit {
        entity: EntityId,
    },
    /// Something a package's player script sent its own game logic (`send(name, data)`):
    /// its `on_message`. `at` is where the player says they are.
    Message {
        package: String,
        name: String,
        data: serde_json::Value,
        at: [f32; 3],
    },
    /// A chat line a player typed that the game itself doesn't know ("/home"): the world
    /// package's `on_chat`.
    Chat {
        text: String,
        at: [f32; 3],
    },
}

/// The most a message may carry, as JSON.
pub const MAX_MESSAGE_BYTES: usize = 2048;

/// What [`Intent::Key`] may carry: directions are the world's (north is -z).
pub const KEYS: [&str; 8] = [
    "north", "south", "east", "west", "action", "reset", "enter", "leave",
];

impl Intent {
    /// Where it happens: a package acts only where it's installed.
    pub fn position(&self, state: &GameState) -> Result<[f32; 3], String> {
        match self {
            Intent::Key { key, .. } if !KEYS.contains(&key.as_str()) => {
                Err(format!("\"{key}\" isn't a key games get"))
            }
            Intent::Interact { entity }
            | Intent::UseItem { target: entity, .. }
            | Intent::Key { entity, .. }
            | Intent::Hit { entity } => state.get(*entity).map(|e| e.position).ok_or(format!(
                "entity {}.{} doesn't exist",
                entity.change, entity.n
            )),
            Intent::Place { position, .. } if position.iter().all(|v| v.is_finite()) => {
                Ok(*position)
            }
            Intent::Place { .. } => Err("that isn't a place".into()),
            // A panel is the player's own, wherever they are.
            Intent::Choose { .. } => Ok([0.0; 3]),
            Intent::Message { name, data, .. }
                if name.is_empty()
                    || name.len() > 64
                    || serde_json::to_vec(data).map_or(true, |b| b.len() > MAX_MESSAGE_BYTES) =>
            {
                Err(format!(
                    "send(name, data): a name up to 64 characters and data up to {MAX_MESSAGE_BYTES} bytes"
                ))
            }
            Intent::Chat { text, .. } if text.trim().is_empty() || text.chars().count() > 300 => {
                Err("a chat line is 1 to 300 characters".into())
            }
            Intent::Message { at, .. } | Intent::Chat { at, .. }
                if at.iter().all(|v| v.is_finite()) =>
            {
                Ok(*at)
            }
            Intent::Message { .. } | Intent::Chat { .. } => Err("that isn't a place".into()),
        }
    }

    /// The package that answers: the item's, or the entity's.
    pub fn package(&self, state: &GameState) -> Option<String> {
        match self {
            Intent::Place { item, .. } => package::split_name(item).map(|(ns, _)| ns.to_string()),
            Intent::Interact { entity }
            | Intent::UseItem { target: entity, .. }
            | Intent::Key { entity, .. }
            | Intent::Hit { entity } => state.get(*entity).map(|e| e.package.clone()),
            // Its package is the open panel's: see [`Logic::prepare`].
            Intent::Choose { .. } => None,
            Intent::Message { package, .. } => Some(package.clone()),
            // The world's own package if it has one, else the first installed.
            Intent::Chat { .. } => None,
        }
    }
}

/// What wakes a package's rules.
#[derive(Clone, Copy, Debug)]
pub enum Trigger<'a> {
    /// A player did something.
    Intent(&'a Intent),
    /// One of its timers went off; `missed` more times went by unfired, if it repeats
    /// and nobody was here to run it.
    Timer {
        name: &'a str,
        data: &'a serde_json::Value,
        missed: u64,
    },
    /// One of its pieces got where it was walking (`stuck`: it found no way there).
    Arrive {
        entity: EntityId,
        stuck: bool,
    },
    /// It was just installed in the world (or a new version of it was: `upgrade`): it
    /// sets the world up. `only`: the script files to start (a new version starts its
    /// new and changed files); all of them if `None`.
    Start {
        upgrade: bool,
        only: Option<&'a [String]>,
        from: Option<u32>,
    },
    /// [`Event::player`] came into the world, or left it.
    Join,
    Leave,
    /// [`Event::player`] walked into the `region` of one of its pieces, or out of it.
    Enter {
        entity: EntityId,
    },
    Exit {
        entity: EntityId,
    },
    /// One of its pieces chasing [`Event::player`] caught up with them.
    Catch {
        entity: EntityId,
    },
    /// Another package told the others something (`emit`): its `on_signal`.
    Signal {
        from: &'a str,
        name: &'a str,
        data: &'a serde_json::Value,
    },
    /// Code to run once with the package's API, for what it returns (an agent asking
    /// about the world: [`Logic::query`]).
    Query {
        code: &'a str,
    },
}

/// What a new version of a package changed in its scripts: the files new or changed (to
/// start), those taken out (their pieces and timers go), and those renamed (their pieces
/// and timers follow), from version `from`.
#[derive(Clone, Debug, Default)]
pub struct Upgrade {
    pub from: u32,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
    /// Old file name, new file name.
    pub renamed: Vec<(String, String)>,
}

impl Upgrade {
    /// From `old` to `new`.
    pub fn between(old: &Package, new: &Package) -> Self {
        let renamed: Vec<(String, String)> = new
            .manifest
            .renamed
            .iter()
            .filter(|(o, n)| old.scripts.contains_key(*o) && new.scripts.contains_key(*n))
            .map(|(o, n)| (o.clone(), n.clone()))
            .collect();
        let changed = new
            .scripts
            .iter()
            .filter(|(f, s)| old.scripts.get(*f) != Some(s))
            .map(|(f, _)| f.clone())
            .collect();
        let removed = old
            .scripts
            .keys()
            .filter(|f| !new.scripts.contains_key(*f) && !renamed.iter().any(|(o, _)| o == *f))
            .cloned()
            .collect();
        Self {
            from: old.manifest.version,
            changed,
            removed,
            renamed,
        }
    }
}

/// What a package's rules are asked about.
#[derive(Clone)]
pub struct Event<'a> {
    /// The ground, for `ground(x, z)`: the seed's terrain with the world's edits (the
    /// same live and in replay). `None` where it isn't known.
    pub ground: Option<&'a dyn Ground>,
    pub change: u64,
    /// Who acted; empty for a timer.
    pub player: &'a str,
    pub trigger: Trigger<'a>,
    pub time: WorldTime,
    /// Unix seconds: what timers count from.
    pub at: u64,
    /// For the package's random numbers: the same for everyone replaying this change.
    pub seed: u64,
}

/// Runs packages' rules. For intents it must answer the same way for the same inputs on
/// every machine, since replay asks it again. Timers are run once, live, and their
/// effects recorded, so replay never asks.
pub trait Runtime: Send + Sync {
    /// Does what `package` does about `event`, through `world` (as `grants` allow). An
    /// error refuses the whole change, and is what the player is told.
    fn handle(
        &self,
        package: &Package,
        event: &Event,
        world: &mut Batch,
        grants: &Grants,
    ) -> Result<(), String>;
}

/// The rules every world runs: a package's scripts (one without any does nothing).
pub struct Rules;

impl Runtime for Rules {
    fn handle(
        &self,
        package: &Package,
        event: &Event,
        world: &mut Batch,
        grants: &Grants,
    ) -> Result<(), String> {
        if package.scripts.is_empty() {
            return Ok(());
        }
        script::run(package, event, world, grants)
    }
}

/// What an intent did.
#[derive(Debug, Default)]
pub struct Outcome {
    /// How many changes the rules made; none means nothing here plays this.
    pub effects: u32,
    /// Messages for players: who, and what.
    pub notes: Vec<(String, String)>,
    /// What it did to the world's voxels, for the world to apply.
    pub world: Vec<WorldChange>,
}

/// What a game does to the world's voxels and portals: the world applies it under its own rules
/// (land, budgets), charged to whoever installed the package.
#[derive(Clone, Debug, PartialEq)]
pub enum WorldChange {
    /// One of the package's models, placed as a solid build.
    Build {
        id: u64,
        package: String,
        model: String,
        spec: crate::build::BuildSpec,
        position: [f32; 3],
        yaw: f32,
    },
    Unbuild {
        id: u64,
    },
    Terrain {
        package: String,
        area: crate::sculpt::Area,
        spec: crate::sculpt::TerrainSpec,
    },
    /// Makes a build the package placed a portal, or (`None`) an ordinary build again.
    Portal {
        id: u64,
        to: Option<crate::log::PortalTo>,
    },
}

/// A package's answer, checked but not applied yet: the world applies its
/// [`WorldChange`]s first, and only then [`Logic::commit`] lands it, so it all lands or
/// none of it does.
pub struct Prepared {
    done: entity::Done,
    pub effects: u32,
    pub notes: Vec<(String, String)>,
    pub world: Vec<WorldChange>,
    /// Who installed the package, and answers for what it does to the world.
    pub installer: String,
    /// Timers have run up to here, when it's what timers did.
    until: Option<u64>,
}

impl Prepared {
    fn nothing() -> Self {
        Self {
            done: GameState::default().batch(0, "", 0).finish(),
            effects: 0,
            notes: Vec::new(),
            world: Vec::new(),
            installer: String::new(),
            until: None,
        }
    }
}

/// An installed package.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Install {
    pub package: PackageRef,
    pub scope: Scope,
    /// Who installed it, and answers for what it does.
    pub by: String,
}

/// The game: packages, what's installed where, and the entities and inventories.
#[derive(Clone, Default)]
pub struct Logic {
    /// Every package the log has, by hash.
    pub packages: BTreeMap<String, Arc<Package>>,
    /// By package name.
    pub installed: BTreeMap<String, Install>,
    pub state: GameState,
    /// What answers intents in place of [`Rules`] (for tests).
    pub runtime: Option<Arc<dyn Runtime>>,
    /// Timers have been run up to here (Unix seconds), as far as the log says.
    pub timers_until: u64,
}

impl Logic {
    /// The game's time at `at`: the world's clock, or where a game set it.
    pub fn time(&self, rules: &LogicRules, at: u64) -> WorldTime {
        match self.state.clock() {
            None => rules.clock.at(at),
            Some((to, set)) => {
                let day = rules.clock.day_seconds.max(1) as f64;
                let shift =
                    ((to - rules.clock.at(set).time_of_day).rem_euclid(1.0) as f64 * day) as u64;
                rules.clock.at(at + shift)
            }
        }
    }

    /// Runs `code` with `package`'s API on the world as it is, as `player`, and returns
    /// what it returns (as JSON) and what it would have changed. Nothing is kept: it's
    /// for looking around, and for trying things out.
    pub fn query(
        &self,
        package: &str,
        player: &str,
        code: &str,
        at: u64,
        rules: &LogicRules,
        ground: Option<&dyn Ground>,
    ) -> Result<(serde_json::Value, u32), String> {
        let install = self
            .installed
            .get(package)
            .ok_or(format!("no package {package} is installed"))?;
        let package = self
            .package(install)
            .ok_or(format!("package {package} is missing"))?;
        let grants = grants(package, install, rules);
        let change = u64::MAX;
        let mut batch = self.state.batch(change, player, rules.max_effects);
        let event = Event {
            ground,
            change,
            player,
            trigger: Trigger::Query { code },
            time: self.time(rules, at),
            at,
            seed: fnv1a(code.as_bytes()),
        };
        script::run(package, &event, &mut batch, &grants)?;
        Ok((
            batch.answer.take().unwrap_or(serde_json::Value::Null),
            batch.applied,
        ))
    }

    /// Checks that `player` may install `package` over `scope` (land aside), and does it.
    pub fn install(
        &mut self,
        player: &str,
        admin: bool,
        rules: &LogicRules,
        package: &PackageRef,
        scope: &Scope,
    ) -> Result<(), String> {
        allowed(admin, rules)?;
        let found = self
            .packages
            .get(&package.hash)
            .ok_or(format!("package {} wasn't uploaded", package.name))?;
        if found.manifest.name != package.name || found.manifest.version != package.version {
            return Err(format!("package {} doesn't match its hash", package.name));
        }
        found.check().map_err(|e| e.join("; "))?;
        scope.check()?;
        if !admin && (*scope == Scope::World || found.needs_admin()) {
            return Err(format!(
                "only an admin can install {} {}",
                package.name,
                if *scope == Scope::World {
                    "everywhere"
                } else {
                    "with permission to change other packages"
                }
            ));
        }
        if let Some(old) = self.installed.get(&package.name) {
            if old.by != player && !admin {
                return Err(format!("{} was installed by someone else", package.name));
            }
            if old.package == *package && old.scope == *scope {
                return Err(format!(
                    "{} v{} is already installed there",
                    package.name, package.version
                ));
            }
        }
        if let Some(dep) = found
            .manifest
            .depends
            .iter()
            .find(|d| !self.installed.contains_key(*d))
        {
            return Err(format!("{} needs {dep} installed first", package.name));
        }
        self.installed.insert(
            package.name.clone(),
            Install {
                package: package.clone(),
                scope: scope.clone(),
                by: player.into(),
            },
        );
        Ok(())
    }

    /// Uninstalls `name`. Its entities stay, dormant, for if it comes back.
    pub fn uninstall(
        &mut self,
        player: &str,
        admin: bool,
        rules: &LogicRules,
        name: &str,
    ) -> Result<(), String> {
        allowed(admin, rules)?;
        let install = self
            .installed
            .get(name)
            .ok_or(format!("{name} isn't installed"))?;
        if install.by != player && !admin {
            return Err(format!("{name} was installed by someone else"));
        }
        let needed = self.installed.values().find(|i| {
            self.package(i)
                .is_some_and(|p| p.manifest.depends.iter().any(|d| d == name))
        });
        if let Some(other) = needed {
            return Err(format!("{} needs {name}", other.package.name));
        }
        self.installed.remove(name);
        Ok(())
    }

    /// Asks the package that owns the intent's item or entity, if it's installed there,
    /// and applies what it does, or nothing if any of it isn't allowed. What it does to
    /// the world's voxels comes back in [`Outcome::world`], unapplied: replay applies it
    /// ([`crate::log::Replay`]), through [`Self::prepare`].
    #[allow(clippy::too_many_arguments)]
    pub fn act(
        &mut self,
        change: u64,
        player: &str,
        at: u64,
        world_seed: u32,
        rules: &LogicRules,
        intent: &Intent,
        ground: &dyn Ground,
    ) -> Result<Outcome, String> {
        let prepared = self.prepare(change, player, at, world_seed, rules, intent, ground)?;
        Ok(self.commit(prepared))
    }

    /// What an intent would do, checked, without doing it.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare(
        &self,
        change: u64,
        player: &str,
        at: u64,
        world_seed: u32,
        rules: &LogicRules,
        intent: &Intent,
        ground: &dyn Ground,
    ) -> Result<Prepared, String> {
        if let Intent::Choose { choice } = intent {
            return self.choose(
                change,
                player,
                at,
                world_seed,
                rules,
                intent,
                choice.as_deref(),
                ground,
            );
        }
        let position = intent.position(&self.state)?;
        let package = match intent {
            Intent::Chat { .. } => self
                .installed
                .get("world")
                .or_else(|| self.installed.values().next())
                .map(|i| i.package.name.clone()),
            _ => intent.package(&self.state),
        };
        let Some(install) = package.and_then(|name| self.installed.get(&name)) else {
            return Ok(Prepared::nothing());
        };
        if !install.scope.contains(position) {
            return Ok(Prepared::nothing());
        }
        let package = self
            .package(install)
            .ok_or(format!("package {} is missing", install.package.name))?;
        let grants = grants(package, install, rules);
        let mut batch = self.state.batch(change, player, rules.max_effects);
        let runtime = self.runtime.clone().unwrap_or_else(|| Arc::new(Rules));
        let seed = fnv1a(format!("{world_seed}/{change}/{}", install.package.name).as_bytes());
        let event = Event {
            ground: Some(ground),
            change,
            player,
            trigger: Trigger::Intent(intent),
            time: self.time(rules, at),
            at,
            seed,
        };
        runtime.handle(package, &event, &mut batch, &grants)?;
        Ok(self.prepared(batch, package, install, None))
    }

    /// A choice in the open panel: closing it, buying in a shop, or the package's
    /// `on_choose`.
    #[allow(clippy::too_many_arguments)]
    fn choose(
        &self,
        change: u64,
        player: &str,
        at: u64,
        world_seed: u32,
        rules: &LogicRules,
        intent: &Intent,
        choice: Option<&str>,
        ground: &dyn Ground,
    ) -> Result<Prepared, String> {
        let panel = self.state.panel(player).ok_or("there's no panel open")?;
        let Some(install) = self.installed.get(&panel.package) else {
            // Its game is gone: closing is all there is to do.
            let grants = Grants {
                package: panel.package.clone(),
                write: Vec::new(),
                spawn: false,
                scope: Scope::World,
                max_entities: 0,
                models: Vec::new(),
                shaders: Vec::new(),
                drawn: false,
                build: false,
            };
            let mut batch = self.state.batch(change, player, rules.max_effects);
            batch.apply(
                &grants,
                &Effect::Show {
                    player: player.into(),
                    panel: None,
                },
            )?;
            let done = batch.finish();
            return Ok(Prepared {
                effects: 1,
                notes: Vec::new(),
                world: Vec::new(),
                installer: String::new(),
                until: None,
                done,
            });
        };
        let package = self
            .package(install)
            .ok_or(format!("package {} is missing", install.package.name))?;
        let grants = grants(package, install, rules);
        let mut batch = self.state.batch(change, player, rules.max_effects);
        match choice {
            None => {
                batch.apply(
                    &grants,
                    &Effect::Show {
                        player: player.into(),
                        panel: None,
                    },
                )?;
            }
            Some(choice) if !panel.spec.body.buttons().contains(&choice) => {
                let offered = panel.spec.body.buttons();
                return Err(format!(
                    "\"{choice}\" isn't a choice in \"{}\" (it offers {})",
                    panel.spec.title,
                    offered.join(", ")
                ));
            }
            Some(_) => {
                let runtime = self.runtime.clone().unwrap_or_else(|| Arc::new(Rules));
                let seed =
                    fnv1a(format!("{world_seed}/{change}/{}", install.package.name).as_bytes());
                let event = Event {
                    ground: Some(ground),
                    change,
                    player,
                    trigger: Trigger::Intent(intent),
                    time: self.time(rules, at),
                    at,
                    seed,
                };
                runtime.handle(package, &event, &mut batch, &grants)?;
            }
        }
        Ok(self.prepared(batch, package, install, None))
    }

    /// A finished batch, with what it does to the world worked out.
    fn prepared(
        &self,
        batch: Batch,
        package: &Package,
        install: &Install,
        until: Option<u64>,
    ) -> Prepared {
        let effects = batch.applied;
        let done = batch.finish();
        let notes = done.notes.clone();
        let name = package.manifest.name.clone();
        let world = done
            .world
            .iter()
            .filter_map(|(effect, id)| match (effect, id) {
                (
                    Effect::Build {
                        model,
                        position,
                        turn,
                    },
                    Some(id),
                ) => package.models.get(model).map(|spec| WorldChange::Build {
                    id: *id,
                    package: name.clone(),
                    model: model.clone(),
                    spec: spec.clone(),
                    position: entity::snap(*position),
                    yaw: *turn as f32 * std::f32::consts::FRAC_PI_2,
                }),
                (Effect::Unbuild { build }, _) => Some(WorldChange::Unbuild { id: *build }),
                (Effect::Terrain { area, spec }, _) => Some(WorldChange::Terrain {
                    package: name.clone(),
                    area: *area,
                    spec: spec.clone(),
                }),
                // Checked when it was staged: it parses.
                (Effect::Portal { build, to }, _) => Some(WorldChange::Portal {
                    id: *build,
                    to: to
                        .as_deref()
                        .and_then(|to| crate::log::PortalTo::parse(to).ok()),
                }),
                _ => None,
            })
            .collect();
        Prepared {
            done,
            effects,
            notes,
            world,
            installer: install.by.clone(),
            until,
        }
    }

    /// Lands what [`Self::prepare`] (or [`Self::prepare_timers`]) worked out.
    pub fn commit(&mut self, prepared: Prepared) -> Outcome {
        self.state.commit(prepared.done);
        if let Some(until) = prepared.until {
            self.timers_until = self.timers_until.max(until);
        }
        Outcome {
            effects: prepared.effects,
            notes: prepared.notes,
            world: prepared.world,
        }
    }

    /// Applies what `package`'s timers did up to `until` (see [`Clockwork::fire`]),
    /// checking each effect again. What they do to the world's voxels isn't applied
    /// here; replay goes through [`Self::prepare_timers`] for that.
    pub fn apply_timers(
        &mut self,
        change: u64,
        package: &str,
        until: u64,
        effects: &[Effect],
        rules: &LogicRules,
    ) -> Result<Vec<(String, String)>, String> {
        let prepared = self.prepare_timers(change, package, until, effects, rules)?;
        Ok(self.commit(prepared).notes)
    }

    /// What `name`'s `on_start` does as it's installed by `player` (or upgraded),
    /// checked, without doing it: it sets the world up (pieces, spawners, builds, where
    /// players appear).
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_start(
        &self,
        change: u64,
        player: &str,
        at: u64,
        world_seed: u32,
        rules: &LogicRules,
        name: &str,
        upgrade: Option<&Upgrade>,
        ground: &dyn Ground,
    ) -> Result<Prepared, String> {
        let install = self
            .installed
            .get(name)
            .ok_or(format!("{name} isn't installed"))?;
        let package = self
            .package(install)
            .ok_or(format!("package {name} is missing"))?;
        let grants = grants(package, install, rules);
        let mut batch = self.state.batch(change, player, rules.max_effects);
        // Features taken out take their pieces and timers with them; renamed ones keep them.
        if let Some(upgrade) = upgrade {
            let feature_of = |e: &Entity| {
                e.components
                    .get("feature")
                    .and_then(|f| f.as_str())
                    .map(str::to_string)
            };
            let pieces: Vec<(EntityId, String)> = self
                .state
                .entities()
                .filter(|(_, e)| e.package == name)
                .filter_map(|(id, e)| feature_of(e).map(|f| (id, f)))
                .collect();
            for (id, feature) in pieces {
                if upgrade.removed.contains(&feature) {
                    batch.apply(&grants, &Effect::Despawn { entity: id })?;
                } else if let Some((_, new)) =
                    upgrade.renamed.iter().find(|(old, _)| *old == feature)
                {
                    batch.apply(
                        &grants,
                        &Effect::Set {
                            entity: id,
                            component: "feature".into(),
                            value: new.clone().into(),
                        },
                    )?;
                }
            }
            let timers: Vec<(String, Timer)> = self
                .state
                .timers_of(name)
                .map(|(n, t)| (n.to_string(), t.clone()))
                .collect();
            for (timer, t) in timers {
                let Some(feature) = &t.feature else { continue };
                if upgrade.removed.contains(feature) {
                    batch.apply(&grants, &Effect::Cancel { name: timer })?;
                } else if let Some((_, new)) =
                    upgrade.renamed.iter().find(|(old, _)| old == feature)
                {
                    let again = Effect::Schedule {
                        name: timer,
                        due: t.due,
                        every: t.every,
                        data: t.data,
                        near: t.near,
                        feature: Some(new.clone()),
                    };
                    batch.apply(&grants, &again)?;
                }
            }
        }
        let runtime = self.runtime.clone().unwrap_or_else(|| Arc::new(Rules));
        let seed = fnv1a(format!("{world_seed}/{change}/{name}/start").as_bytes());
        let trigger = Trigger::Start {
            upgrade: upgrade.is_some(),
            only: upgrade.map(|u| u.changed.as_slice()),
            from: upgrade.map(|u| u.from),
        };
        let event = Event {
            ground: Some(ground),
            change,
            player,
            trigger,
            time: self.time(rules, at),
            at,
            seed,
        };
        runtime.handle(package, &event, &mut batch, &grants)?;
        Ok(self.prepared(batch, package, install, None))
    }

    /// What `package`'s timers did, checked, without doing it.
    pub fn prepare_timers(
        &self,
        change: u64,
        package: &str,
        until: u64,
        effects: &[Effect],
        rules: &LogicRules,
    ) -> Result<Prepared, String> {
        let install = self
            .installed
            .get(package)
            .ok_or(format!("{package} isn't installed"))?;
        let found = self
            .package(install)
            .ok_or(format!("package {package} is missing"))?;
        let grants = grants(found, install, rules);
        let mut batch = self.state.batch(change, "", rules.max_effects);
        for effect in effects {
            batch.apply(&grants, effect)?;
        }
        Ok(self.prepared(batch, found, install, Some(until)))
    }

    /// Whether players may build: not while an installed game says the world is its
    /// own (a piece with `world = {building = false}`).
    pub fn building_allowed(&self) -> bool {
        !self.state.entities().any(|(_, e)| {
            self.installed.contains_key(&e.package)
                && e.components
                    .get("world")
                    .is_some_and(|w| w["building"] == false)
        })
    }

    pub fn package(&self, install: &Install) -> Option<&Package> {
        self.packages.get(&install.package.hash).map(|p| &**p)
    }
}

fn allowed(admin: bool, rules: &LogicRules) -> Result<(), String> {
    match rules.mode {
        LogicMode::Locked => Err("this world's game rules are locked".into()),
        LogicMode::AdminOnly if !admin => Err("only admins change this world's game rules".into()),
        _ => Ok(()),
    }
}

/// What an installed package may touch.
pub fn grants(package: &Package, install: &Install, rules: &LogicRules) -> Grants {
    let perms = &package.manifest.permissions;
    Grants {
        package: package.manifest.name.clone(),
        write: perms
            .iter()
            .filter_map(|p| match p {
                Permission::Write { namespace } => Some(namespace.clone()),
                _ => None,
            })
            .collect(),
        spawn: perms.contains(&Permission::Spawn),
        scope: install.scope.clone(),
        max_entities: rules.max_entities,
        models: package.models.keys().cloned().collect(),
        shaders: package.shaders.keys().cloned().collect(),
        drawn: !package.scripts.is_empty(),
        build: perms.contains(&Permission::Build),
    }
}
