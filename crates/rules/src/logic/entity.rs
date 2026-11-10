//! The game's state: entities (boxes, walls, crops, NPCs) and what each player carries.
//! Each entity belongs to the package that spawned it and carries named components
//! (`sokoban:box`) whose values are plain JSON; items are named the same way.
//!
//! A few components belong to the engine, which acts on them, and only an entity's own
//! package sets them:
//! - `look`: how it's drawn, `{"model": "crate", "turn": 0}`: one of its package's voxel
//!   models (see [`super::Package::models`]), standing on its position, turned by
//!   quarter turns. Nothing else draws game pieces, so they all share the world's style.
//! - `label`: a name shown over it when a player is near.
//! - `pawn`: a player id. That player's hero stands where this entity is, so a game
//!   moves its players by moving their pawns.
//! - `walk`, `chase`: pieces that move on their own (see [`super::walk`]).
//! - `region`: `{"radius": r}` or `{"size": [w, d]}` around it, and `"keys": true` to
//!   take the movement keys of a player standing in it on their pawn (as
//!   [`super::Intent::Key`]): players walking in and out
//!   tell its package (`on_enter`, `on_exit`). It needs no look.
//! - `world`: `{"building": false}`: while a piece with it stands, only admins build and
//!   reshape land; the world is the game. It needs no look.
//! - `feature`: the script file that spawned it (set for it): taking that file out of
//!   the package takes the piece out too.
//!
//! Players have state of their own too, also kept by package: **stats** (`game:hp`,
//! numbers kept between 0 and a maximum when they have one), **HUD widgets** pinned to
//! their screen (a bar for a stat, a counter, a line of text), and an open **panel**
//! (dialogue, a menu, a shop: a title, text, and choices, answered through
//! [`super::Intent::Choose`]).
//!
//! Games change voxels through the world's own operations ([`Effect::Build`],
//! [`Effect::Unbuild`], [`Effect::Terrain`]): the batch checks and collects them, and the
//! world applies them under its land rules and the installer's budget
//! ([`crate::log::Replay`]).
//!
//! Packages never write here directly. Each change goes through a [`Batch`], which checks
//! it against what the package may do; the batch lands whole, or not at all.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    Scope,
    package::split_name,
    present::{Appearance, Show, Shown, Sky},
    solid::Areas,
    ui::{MAX_HUD, PanelSpec, Ui},
};
use crate::sculpt::{Area, TerrainSpec};

/// The most one component's value may weigh, as JSON.
pub const MAX_COMPONENT_BYTES: usize = 4096;
/// The most of one item a player may carry.
pub const MAX_ITEMS: u32 = 9999;
/// Entities stand on the build voxel grid (12.5 cm), like builds.
pub const GRID: f32 = crate::build::VOXEL_SIZE;

/// The grid point nearest `p`.
pub fn snap(p: [f32; 3]) -> [f32; 3] {
    p.map(|v| (v / GRID).round() * GRID)
}

/// Named by the change that spawned it and its place among that change's spawns, so
/// undoing an earlier change never renames a later one's entities.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EntityId {
    pub change: u64,
    pub n: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entity {
    /// The package that spawned it.
    pub package: String,
    /// In meters, like builds.
    pub position: [f32; 3],
    pub components: BTreeMap<String, Value>,
}

/// One thing a package's rules want done.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Effect {
    Spawn {
        position: [f32; 3],
        #[serde(default)]
        components: BTreeMap<String, Value>,
    },
    /// Removes one of the package's own entities.
    Despawn { entity: EntityId },
    /// Moves one of the package's own entities.
    Move { entity: EntityId, position: [f32; 3] },
    Set { entity: EntityId, component: String, value: Value },
    Remove { entity: EntityId, component: String },
    /// Gives a player items.
    Give { player: String, item: String, count: u32 },
    /// Takes items from the player acting, failing if they don't have them.
    Take { item: String, count: u32 },
    /// Tells a player something ("Solved in 14 moves!"). Changes nothing.
    Notify { player: String, text: String },
    /// Wakes the package's `on_timer` at `due` (Unix seconds), and again every `every`
    /// seconds after if given. A timer of the same name is replaced. With `near`, it
    /// sleeps while no player is near that entity (see [`Timer::near`]).
    Schedule {
        name: String,
        due: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        every: Option<u64>,
        #[serde(default)]
        data: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        near: Option<EntityId>,
        /// The script file that set it: taking that file out cancels it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        feature: Option<String>,
    },
    /// Stops one of the package's timers, if it has it.
    Cancel { name: String },
    /// Sets one of a player's stats, kept between 0 and its maximum (`max` sets that; none
    /// keeps the one it has).
    SetStat {
        player: String,
        stat: String,
        value: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<f64>,
    },
    /// Pins a widget to a player's screen (`"*"`: everyone's), or takes it off (`None`).
    Hud { player: String, id: String, widget: Option<Ui> },
    /// Opens a panel for a player (dialogue, a menu, a shop), or closes theirs (`None`).
    Show { player: String, panel: Option<PanelSpec> },
    /// Places one of the package's voxel models as a build: solid, like any build.
    Build { model: String, position: [f32; 3], #[serde(default)] turn: u8 },
    /// Removes a build the package placed.
    Unbuild { build: u64 },
    /// Reshapes land with the world's terrain operations.
    Terrain { area: Area, spec: TerrainSpec },
    /// Puts a player's hero at `position` (a respawn, a trapdoor, a portal of the game's).
    /// For `"*"`, it's where players appear when they come into the world.
    Teleport { player: String, position: [f32; 3] },
    /// Sends a player to another world of the game (a dungeon's door, the way home):
    /// they leave this one with what they carry, as through a portal.
    Travel { player: String, to: String },
    /// Makes a build the package placed a portal (to a world, `game/world` or
    /// `host:port`), or an ordinary build again (`None`).
    Portal { build: u64, to: Option<String> },
    /// Changes how a player's hero moves (`None`: back to normal).
    Body { player: String, body: Option<Body> },
    /// Throws a player's hero: sets how fast it's going, meters a second.
    Launch { player: String, velocity: [f32; 3] },
    /// Shows or plays something once at `pos`, for everyone or one player.
    Play { pos: [f32; 3], to: Option<String>, what: Shown },
    /// How the sky looks (`None`: the world's own again).
    Sky { sky: Option<Sky> },
    /// What a player's hero looks like (`None`: their own again).
    Appearance { player: String, appearance: Option<Appearance> },
    /// Tells the other packages something happened (their `on_signal`, at their next tick).
    Emit { name: String, data: Value },
    /// The package has heard the signals up to this change.
    Heard { upto: u64 },
    /// Keeps a value for the package under `key` (`None` forgets it).
    Store { key: String, value: Option<Value> },
    /// Sets the game's time of day (0-1) as of `at` (Unix seconds); `None` goes back to the
    /// world's own clock.
    Clock { to: Option<f32>, at: u64 },
}

/// Something one package told the others.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Signal {
    pub change: u64,
    pub from: String,
    pub name: String,
    pub data: Value,
}

/// How many signals the world keeps for packages that haven't heard them yet.
pub const MAX_SIGNALS: usize = 64;

/// How many recent effects and sounds the world keeps for players' games to play.
pub const MAX_SHOWS: usize = 32;

/// Where a game last put a player, and in which change (so a game shows each move once).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Teleport {
    pub position: [f32; 3],
    pub change: u64,
}

/// Where a game last sent a player, out of this world, and in which change (so a game
/// sends them each time once).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Travel {
    pub to: String,
    pub change: u64,
}

/// How a game has a player's hero move, each a multiple of the usual.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Body {
    #[serde(default = "one")]
    pub speed: f32,
    #[serde(default = "one")]
    pub jump: f32,
    #[serde(default = "one")]
    pub gravity: f32,
    /// Jumps they can make in the air, after leaving the ground.
    #[serde(default)]
    pub air_jumps: u8,
}

fn one() -> f32 {
    1.0
}

impl Default for Body {
    fn default() -> Self {
        Self { speed: 1.0, jump: 1.0, gravity: 1.0, air_jumps: 0 }
    }
}

impl Body {
    fn check(&self) -> Result<(), String> {
        let within = |v: f32, lo: f32, hi: f32| v.is_finite() && (lo..=hi).contains(&v);
        if within(self.speed, 0.0, 4.0) && within(self.jump, 0.0, 4.0) && within(self.gravity, 0.1, 2.0) && self.air_jumps <= 3 {
            Ok(())
        } else {
            Err("body(player, {speed = 0-4, jump = 0-4, gravity = 0.1-2, air_jumps = 0-3}): multiples of the usual".into())
        }
    }
}

/// How a game last threw a player, and in which change (so it's felt once).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Launch {
    pub velocity: [f32; 3],
    pub change: u64,
}

/// A number a player has (hit points, gold, hunger).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stat {
    pub value: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
}

/// An open panel: what the package asked for, and whose it is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Panel {
    pub package: String,
    #[serde(flatten)]
    pub spec: PanelSpec,
}

/// Builds a package places get ids from here up, so they never meet players' own.
pub const SCRIPT_BUILDS: u64 = 1 << 52;

/// A package's wakeup: see [`Effect::Schedule`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Timer {
    pub due: u64,
    pub every: Option<u64>,
    pub data: Value,
    /// Runs only while a player is within [`WAKE_RADIUS`] of this entity: a game board's
    /// timers sleep while nobody is there, and catch up when someone comes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub near: Option<EntityId>,
    /// The script file that set it, if one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature: Option<String>,
}

/// How near a player must be for a timer anchored to an entity to run, in meters.
pub const WAKE_RADIUS: f32 = 64.0;

/// The most timers one package may have waiting.
pub const MAX_TIMERS: usize = 256;
/// The shortest a timer may wait, and repeat, in seconds.
pub const MIN_WAIT: u64 = 1;

impl Timer {
    /// When it goes off by `now`, if it does, and how many times it would have. A
    /// one-time timer goes off once it's due (and is gone after). A repeating one goes
    /// off for its times in `(since, now]`, folded into one firing when it fell behind.
    pub fn fires(&self, since: u64, now: u64) -> Option<(u64, u64)> {
        match self.every {
            _ if self.due > now => None,
            None => Some((self.due, 1)),
            Some(every) => {
                let every = every.max(MIN_WAIT);
                let last = self.due + (now - self.due) / every * every;
                let first = if self.due > since { self.due } else { self.due + ((since - self.due) / every + 1) * every };
                (first <= last).then(|| (last, (last - first) / every + 1))
            }
        }
    }

    /// When it next goes off after `since`, if ever.
    pub fn next(&self, since: u64) -> Option<u64> {
        match self.every {
            _ if self.due > since => Some(self.due),
            None => Some(self.due),
            Some(every) => {
                let every = every.max(MIN_WAIT);
                Some(self.due + ((since - self.due) / every + 1) * every)
            }
        }
    }
}

/// Components the engine itself reads.
/// Engine components the game no longer has, and what games use instead.
pub const REMOVED_COMPONENTS: [(&str, &str); 3] = [
    ("wander", "walk() from on_arrive"),
    ("spawner", "an every() timer that spawns while find() says there are too few"),
    ("zone", "region(id, {w, d, keys = true})"),
];

pub const RESERVED: [&str; 9] = ["look", "decal", "label", "pawn", "walk", "chase", "region", "world", "feature"];

/// Checks an engine component's value, phrased for the agent that wrote it.
fn check_reserved(name: &str, value: &Value, grants: &Grants) -> Result<(), String> {
    let sizes = |key: &str, n: usize, max: f64| {
        let list = value[key].as_array().filter(|a| a.len() == n);
        list.is_some_and(|a| a.iter().all(|v| v.as_f64().is_some_and(|v| v > 0.0 && v <= max)))
    };
    let ok = match name {
        "look" => {
            let model = value["model"].as_str().is_some_and(|m| grants.models.iter().any(|g| g == m));
            let turn = value["turn"].is_null() || value["turn"].as_u64().is_some_and(|t| t < 4);
            let solid = value["solid"].is_null() || value["solid"].is_boolean();
            let scale = value["scale"].is_null() || value["scale"].as_f64().is_some_and(|s| (0.1..=8.0).contains(&s));
            let tint = value["tint"].is_null() || value["tint"].as_str().is_some_and(super::present::is_color);
            let flags = ["glow", "hidden"].iter().all(|k| value[*k].is_null() || value[*k].is_boolean());
            model && turn && solid && scale && tint && flags
        }
        "decal" => super::shader::DecalLook::read(value, &grants.shaders).is_some(),
        "label" => value.as_str().is_some_and(|l| l.chars().count() <= 60),
        "pawn" => value.as_str().is_some_and(|p| !p.is_empty() && p.len() <= 128),
        "feature" => value.as_str().is_some_and(|s| s.len() <= 64),
        "walk" | "chase" => return super::walk::check(name, value),
        "region" => {
            (value["radius"].as_f64().is_some_and(|r| r > 0.0 && r <= 64.0) || sizes("size", 2, 128.0))
                && (value["keys"].is_null() || value["keys"].is_boolean())
        }
        "world" => value.as_object().is_some_and(|o| o.keys().all(|k| k == "building")) && !value["building"].is_null(),
        _ => true,
    };
    if ok {
        Ok(())
    } else {
        Err(match name {
            "look" => format!(
                "\"look\" is {{\"model\": one of the package's models ({}), \"turn\": 0-3, \"solid\": true to block walking, \"scale\": 0.1-8, \"tint\": \"#rrggbb\", \"glow\": true, \"hidden\": true}}",
                if grants.models.is_empty() { "it has none".to_string() } else { grants.models.join(", ") }
            ),
            "decal" => format!(
                "{} ({})",
                super::shader::DecalLook::HELP,
                if grants.shaders.is_empty() { "it has no shaders".to_string() } else { format!("its shaders: {}", grants.shaders.join(", ")) }
            ),
            "label" => "\"label\" is text of up to 60 characters".into(),
            "region" => "\"region\" is {\"radius\": 0-64 m} or {\"size\": [w, d]} (meters, 0-128), and \"keys\": true to take the keys".into(),
            "world" => "\"world\" is {\"building\": true or false}".into(),
            _ => "\"pawn\" is a player id".into(),
        })
    }
}

/// What one installed package may touch.
#[derive(Clone, Debug)]
pub struct Grants {
    pub package: String,
    pub write: Vec<String>,
    pub spawn: bool,
    pub scope: Scope,
    pub max_entities: u32,
    /// Its voxel models, which its entities' `look` may name.
    pub models: Vec<String>,
    /// Its shaders, which its entities' `decal` may name.
    pub shaders: Vec<String>,
    /// Every entity it makes must be drawn: a `look`, unless it's a `pawn` (the hero
    /// shows those). True for scripted packages; the simple farming kinds draw
    /// themselves.
    pub drawn: bool,
    /// It may place builds and reshape land ([`super::Permission::Build`]).
    pub build: bool,
}

impl Grants {
    /// Whether it may change the component or item `name`.
    fn writable(&self, name: &str) -> Result<(), String> {
        let Some((ns, _)) = split_name(name) else {
            // Engine components the game had once (recorded in old changes).
            if let Some((_, instead)) = REMOVED_COMPONENTS.iter().find(|(old, _)| *old == name) {
                return Err(format!("the engine no longer has \"{name}\"; games use {instead}"));
            }
            return Err(format!("\"{name}\" must be named \"<package>:<name>\""));
        };
        if ns == self.package || self.write.iter().any(|w| w == ns) {
            Ok(())
        } else {
            Err(format!("{} may not change \"{name}\", which belongs to {ns}", self.package))
        }
    }

    /// Whether it may read the stat or component `name`: its own, or one it may write or
    /// read.
    fn readable(&self, name: &str) -> Result<(), String> {
        self.writable(name)
    }

    fn inside(&self, position: [f32; 3]) -> Result<(), String> {
        if self.scope.contains(position) {
            Ok(())
        } else {
            Err(format!("{} works only where it's installed", self.package))
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "SavedState", into = "SavedState")]
pub struct GameState {
    entities: BTreeMap<EntityId, Entity>,
    /// Entities per package, for its limit.
    counts: BTreeMap<String, u32>,
    /// What each player carries: item name to how many (never 0).
    items: BTreeMap<String, BTreeMap<String, u32>>,
    /// Packages' timers, by package and name.
    timers: BTreeMap<(String, String), Timer>,
    /// Players' stats, by player and stat.
    stats: BTreeMap<String, BTreeMap<String, Stat>>,
    /// What's pinned to players' screens, by player (`"*"`: everyone) and `package:id`.
    hud: BTreeMap<String, BTreeMap<String, Ui>>,
    /// The panel each player has open.
    panels: BTreeMap<String, Panel>,
    /// Builds packages placed, and which package.
    builds: BTreeMap<u64, String>,
    /// Where games last put each player (`"*"`: where players appear).
    teleports: BTreeMap<String, Teleport>,
    travels: BTreeMap<String, Travel>,
    /// Players' names, as they signed their changes.
    names: BTreeMap<String, String>,
    /// How games have players move, when it isn't the usual.
    bodies: BTreeMap<String, Body>,
    /// How games last threw each player.
    launches: BTreeMap<String, Launch>,
    /// How a game has the sky look.
    sky: Option<Sky>,
    /// What games show players' heroes as.
    appearances: BTreeMap<String, Appearance>,
    /// Effects and sounds lately, newest last (players' games play each once).
    shows: Vec<Show>,
    /// What packages told each other lately (`emit`), oldest first.
    signals: Vec<Signal>,
    /// The last signal each package has heard.
    heard: BTreeMap<String, u64>,
    /// The time of day a game set, and when (Unix seconds): the game clock runs on from it.
    clock: Option<(f32, u64)>,
    /// What each package keeps for itself (`remember`), by key.
    stores: BTreeMap<String, BTreeMap<String, Value>>,
    /// Entities by where they stand.
    areas: Areas<EntityId>,
}

/// [`GameState`] as saved in snapshots: JSON maps need string keys.
#[derive(Serialize, Deserialize)]
struct SavedState {
    entities: Vec<(EntityId, Entity)>,
    #[serde(default)]
    items: BTreeMap<String, BTreeMap<String, u32>>,
    #[serde(default)]
    timers: Vec<(String, String, Timer)>,
    #[serde(default)]
    stats: BTreeMap<String, BTreeMap<String, Stat>>,
    #[serde(default)]
    hud: BTreeMap<String, BTreeMap<String, Ui>>,
    #[serde(default)]
    panels: BTreeMap<String, Panel>,
    #[serde(default)]
    builds: Vec<(u64, String)>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    teleports: BTreeMap<String, Teleport>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    travels: BTreeMap<String, Travel>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    names: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    bodies: BTreeMap<String, Body>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    launches: BTreeMap<String, Launch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sky: Option<Sky>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    appearances: BTreeMap<String, Appearance>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    shows: Vec<Show>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    signals: Vec<Signal>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    heard: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    clock: Option<(f32, u64)>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    stores: BTreeMap<String, BTreeMap<String, Value>>,
}

impl From<GameState> for SavedState {
    fn from(state: GameState) -> Self {
        Self {
            entities: state.entities.into_iter().collect(),
            items: state.items,
            timers: state.timers.into_iter().map(|((p, n), t)| (p, n, t)).collect(),
            stats: state.stats,
            hud: state.hud,
            panels: state.panels,
            builds: state.builds.into_iter().collect(),
            teleports: state.teleports,
            travels: state.travels,
            names: state.names,
            bodies: state.bodies,
            launches: state.launches,
            sky: state.sky,
            appearances: state.appearances,
            shows: state.shows,
            signals: state.signals,
            heard: state.heard,
            clock: state.clock,
            stores: state.stores,
        }
    }
}

impl From<SavedState> for GameState {
    fn from(saved: SavedState) -> Self {
        let mut counts: BTreeMap<String, u32> = BTreeMap::new();
        for (_, e) in &saved.entities {
            *counts.entry(e.package.clone()).or_default() += 1;
        }
        let mut areas = Areas::default();
        for (id, e) in &saved.entities {
            areas.put(*id, e.position, 0.0);
        }
        Self {
            entities: saved.entities.into_iter().collect(),
            areas,
            counts,
            items: saved.items,
            timers: saved.timers.into_iter().map(|(p, n, t)| ((p, n), t)).collect(),
            stats: saved.stats,
            hud: saved.hud,
            panels: saved.panels,
            builds: saved.builds.into_iter().collect(),
            teleports: saved.teleports,
            travels: saved.travels,
            names: saved.names,
            bodies: saved.bodies,
            launches: saved.launches,
            sky: saved.sky,
            appearances: saved.appearances,
            shows: saved.shows,
            signals: saved.signals,
            heard: saved.heard,
            clock: saved.clock,
            stores: saved.stores,
        }
    }
}

impl GameState {
    pub fn get(&self, id: EntityId) -> Option<&Entity> {
        self.entities.get(&id)
    }

    pub fn entities(&self) -> impl Iterator<Item = (EntityId, &Entity)> {
        self.entities.iter().map(|(id, e)| (*id, e))
    }

    /// Entities standing on the ground from `lo` to `hi` (x, z), and maybe a few more
    /// near it.
    pub fn within(&self, lo: [f32; 2], hi: [f32; 2]) -> impl Iterator<Item = (EntityId, &Entity)> {
        self.areas.within(lo, hi).into_iter().filter_map(|id| Some((id, self.entities.get(&id)?)))
    }

    pub fn len(&self) -> usize {
        self.entities.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    pub fn count(&self, package: &str) -> u32 {
        self.counts.get(package).copied().unwrap_or(0)
    }

    /// How many of `item` `player` carries.
    pub fn items(&self, player: &str, item: &str) -> u32 {
        self.items.get(player).and_then(|i| i.get(item)).copied().unwrap_or(0)
    }

    /// Everything `player` carries.
    pub fn inventory(&self, player: &str) -> impl Iterator<Item = (&str, u32)> {
        self.items.get(player).into_iter().flatten().map(|(item, n)| (item.as_str(), *n))
    }

    /// One of `player`'s stats.
    pub fn stat(&self, player: &str, stat: &str) -> Option<Stat> {
        self.stats.get(player).and_then(|s| s.get(stat)).copied()
    }

    /// All of `player`'s stats.
    pub fn stats(&self, player: &str) -> impl Iterator<Item = (&str, Stat)> {
        self.stats.get(player).into_iter().flatten().map(|(n, s)| (n.as_str(), *s))
    }

    /// The widgets on `player`'s screen: everyone's, then their own.
    pub fn hud(&self, player: &str) -> impl Iterator<Item = (&str, &Ui)> {
        ["*", player].into_iter().flat_map(|p| self.hud.get(p).into_iter().flatten()).map(|(id, w)| (id.as_str(), w))
    }

    /// The panel `player` has open.
    pub fn panel(&self, player: &str) -> Option<&Panel> {
        self.panels.get(player)
    }

    /// Every player the world keeps something for: items, stats, a HUD, a panel, a
    /// teleport. For games that change how they keep players' things (a migration).
    pub fn players(&self) -> Vec<String> {
        let mut out: std::collections::BTreeSet<&String> = self.items.keys().collect();
        out.extend(self.stats.keys());
        out.extend(self.hud.keys().filter(|p| *p != "*"));
        out.extend(self.panels.keys());
        out.extend(self.teleports.keys().filter(|p| *p != "*"));
        out.extend(self.bodies.keys());
        out.into_iter().cloned().collect()
    }

    /// Where a game last put `player` (`"*"`: where players appear), and when.
    pub fn teleport(&self, player: &str) -> Option<Teleport> {
        self.teleports.get(player).copied()
    }

    /// The time of day a game set, and when.
    /// Where a game last sent `player`, out of this world.
    pub fn travel(&self, player: &str) -> Option<&Travel> {
        self.travels.get(player)
    }

    pub fn clock(&self) -> Option<(f32, u64)> {
        self.clock
    }

    /// What `player` is called (their name as they signed their changes).
    pub fn name(&self, player: &str) -> Option<&str> {
        self.names.get(player).map(String::as_str)
    }

    /// Notes what `player` is called: replay does, from each change's author.
    pub fn set_name(&mut self, player: &str, name: &str) {
        if self.names.get(player).is_none_or(|n| n != name) && !name.trim().is_empty() {
            self.names.insert(player.to_string(), name.chars().take(40).collect());
        }
    }

    /// How `player`'s hero moves.
    pub fn body(&self, player: &str) -> Body {
        self.bodies.get(player).copied().unwrap_or_default()
    }

    /// How a game last threw `player`, and when.
    pub fn launch(&self, player: &str) -> Option<Launch> {
        self.launches.get(player).copied()
    }

    /// How a game has the sky look, if one does.
    pub fn sky(&self) -> Option<&Sky> {
        self.sky.as_ref()
    }

    /// What a game shows `player`'s hero as, if one does.
    pub fn appearance(&self, player: &str) -> Option<&Appearance> {
        self.appearances.get(player)
    }

    /// Effects and sounds lately, oldest first.
    pub fn shows(&self) -> &[Show] {
        &self.shows
    }

    /// Signals from other packages that `package` hasn't heard yet.
    pub fn unheard(&self, package: &str) -> impl Iterator<Item = &Signal> {
        let since = self.heard.get(package).copied().unwrap_or(0);
        self.signals.iter().filter(move |s| s.change > since && s.from != package)
    }

    /// What `package` keeps under `key`.
    pub fn recalled(&self, package: &str, key: &str) -> Option<&Value> {
        self.stores.get(package)?.get(key)
    }

    /// The package that placed build `id`, if a package did.
    pub fn build_owner(&self, id: u64) -> Option<&str> {
        self.builds.get(&id).map(String::as_str)
    }

    /// Forgets a build (removed by its package, or by a player).
    pub fn forget_build(&mut self, id: u64) {
        self.builds.remove(&id);
    }

    /// Everything `player` carries, to take to another world.
    pub fn carried(&self, player: &str) -> Vec<(String, u32)> {
        self.inventory(player).map(|(item, n)| (item.to_string(), n)).collect()
    }

    /// Takes `items` from `player`, who's leaving for another world with them.
    pub fn carry_out(&mut self, player: &str, items: &[(String, u32)]) -> Result<(), String> {
        for (item, n) in items {
            if self.items(player, item) < *n {
                return Err(format!("{player} doesn't carry {n} \"{item}\""));
            }
        }
        let carried = self.items.entry(player.to_string()).or_default();
        for (item, n) in items {
            let left = carried.get(item).copied().unwrap_or(0) - n;
            if left == 0 {
                carried.remove(item);
            } else {
                carried.insert(item.clone(), left);
            }
        }
        if carried.is_empty() {
            self.items.remove(player);
        }
        Ok(())
    }

    /// `player`'s stats, which go with them to another world.
    pub fn carried_stats(&self, player: &str) -> Vec<(String, Stat)> {
        self.stats(player).map(|(n, s)| (n.to_string(), s)).collect()
    }

    /// Moves stats with a player: away (`None`) or in.
    pub fn carry_stats(&mut self, player: &str, stats: Option<&[(String, Stat)]>) {
        match stats {
            None => {
                self.stats.remove(player);
                self.panels.remove(player);
            }
            Some(stats) => {
                let own = self.stats.entry(player.to_string()).or_default();
                for (name, stat) in stats {
                    own.insert(name.clone(), *stat);
                }
                if own.is_empty() {
                    self.stats.remove(player);
                }
            }
        }
    }

    /// Gives `player` what they carried here from another world.
    pub fn carry_in(&mut self, player: &str, items: &[(String, u32)]) {
        let carried = self.items.entry(player.to_string()).or_default();
        for (item, n) in items.iter().filter(|(_, n)| *n > 0) {
            let have = carried.entry(item.clone()).or_default();
            *have = have.saturating_add(*n).min(MAX_ITEMS);
        }
        if carried.is_empty() {
            self.items.remove(player);
        }
    }

    /// Every waiting timer: package, name, timer.
    pub fn timers(&self) -> impl Iterator<Item = (&str, &str, &Timer)> {
        self.timers.iter().map(|((p, n), t)| (p.as_str(), n.as_str(), t))
    }

    /// `package`'s waiting timers.
    pub fn timers_of<'a>(&'a self, package: &'a str) -> impl Iterator<Item = (&'a str, &'a Timer)> {
        self.timers.range((package.to_string(), String::new())..).take_while(move |((p, _), _)| p == package).map(|((_, n), t)| (n.as_str(), t))
    }

    /// Starts a set of effects for change `change` by `actor`, at most `max_effects`,
    /// none applied until [`Batch::finish`].
    pub fn batch<'a>(&'a self, change: u64, actor: &'a str, max_effects: u32) -> Batch<'a> {
        Batch {
            base: self,
            change,
            actor,
            max_effects,
            applied: 0,
            spawned: 0,
            touched: BTreeMap::new(),
            counts: BTreeMap::new(),
            items: BTreeMap::new(),
            notes: Vec::new(),
            timers: BTreeMap::new(),
            effects: Vec::new(),
            stats: BTreeMap::new(),
            hud: BTreeMap::new(),
            panels: BTreeMap::new(),
            builds: BTreeMap::new(),
            built: 0,
            last_build: None,
            answer: None,
            teleports: BTreeMap::new(),
            travels: BTreeMap::new(),
            bodies: BTreeMap::new(),
            launches: BTreeMap::new(),
            sky: None,
            appearances: BTreeMap::new(),
            shows: Vec::new(),
            signals: Vec::new(),
            heard: BTreeMap::new(),
            clock: None,
            stores: BTreeMap::new(),
        }
    }

    /// Lands a finished batch.
    pub fn commit(&mut self, done: Done) {
        for (id, entity) in done.touched {
            match entity {
                Some(e) => {
                    self.areas.put(id, e.position, 0.0);
                    self.entities.insert(id, e)
                }
                None => {
                    self.areas.take(id);
                    self.entities.remove(&id)
                }
            };
        }
        for (package, delta) in done.counts {
            let count = self.counts.entry(package).or_default();
            *count = count.saturating_add_signed(delta);
        }
        for ((player, item), n) in done.items {
            let carried = self.items.entry(player.clone()).or_default();
            if n == 0 {
                carried.remove(&item);
            } else {
                carried.insert(item, n);
            }
            if carried.is_empty() {
                self.items.remove(&player);
            }
        }
        for (key, timer) in done.timers {
            match timer {
                Some(t) => self.timers.insert(key, t),
                None => self.timers.remove(&key),
            };
        }
        for ((player, name), stat) in done.stats {
            self.stats.entry(player).or_default().insert(name, stat);
        }
        for ((player, id), widget) in done.hud {
            let screen = self.hud.entry(player.clone()).or_default();
            match widget {
                Some(w) => screen.insert(id, w),
                None => screen.remove(&id),
            };
            if screen.is_empty() {
                self.hud.remove(&player);
            }
        }
        for (player, panel) in done.panels {
            match panel {
                Some(p) => self.panels.insert(player, p),
                None => self.panels.remove(&player),
            };
        }
        for (id, owner) in done.builds {
            match owner {
                Some(package) => self.builds.insert(id, package),
                None => self.builds.remove(&id),
            };
        }
        self.teleports.extend(done.teleports);
        self.travels.extend(done.travels);
        for (player, body) in done.bodies {
            match body {
                Some(b) => self.bodies.insert(player, b),
                None => self.bodies.remove(&player),
            };
        }
        self.launches.extend(done.launches);
        if let Some(sky) = done.sky {
            self.sky = sky;
        }
        for (player, look) in done.appearances {
            match look {
                Some(a) => self.appearances.insert(player, a),
                None => self.appearances.remove(&player),
            };
        }
        self.shows.extend(done.shows);
        let excess = self.shows.len().saturating_sub(MAX_SHOWS);
        self.shows.drain(..excess);
        self.signals.extend(done.signals);
        let excess = self.signals.len().saturating_sub(MAX_SIGNALS);
        self.signals.drain(..excess);
        self.heard.extend(done.heard);
        if let Some(clock) = done.clock {
            self.clock = clock;
        }
        for ((package, key), value) in done.stores {
            let store = self.stores.entry(package.clone()).or_default();
            match value {
                Some(v) => store.insert(key, v),
                None => store.remove(&key),
            };
            if store.is_empty() {
                self.stores.remove(&package);
            }
        }
    }
}

/// Effects checked and staged on top of the state, all or nothing. Reads see what's
/// staged so far, so rules see their own changes.
pub struct Batch<'a> {
    base: &'a GameState,
    change: u64,
    actor: &'a str,
    max_effects: u32,
    /// Effects staged so far.
    pub applied: u32,
    spawned: u32,
    /// Entities changed so far (`None`: removed).
    touched: BTreeMap<EntityId, Option<Entity>>,
    counts: BTreeMap<String, i32>,
    /// New item counts, by player and item.
    items: BTreeMap<(String, String), u32>,
    notes: Vec<(String, String)>,
    /// Timers set (`None`: cancelled), by package and name.
    timers: BTreeMap<(String, String), Option<Timer>>,
    /// Every effect staged, in order: what a timer's firing records in the log.
    pub effects: Vec<Effect>,
    stats: BTreeMap<(String, String), Stat>,
    hud: BTreeMap<(String, String), Option<Ui>>,
    panels: BTreeMap<String, Option<Panel>>,
    /// Builds placed (`Some(package)`) and removed (`None`).
    builds: BTreeMap<u64, Option<String>>,
    /// Builds placed so far, for their ids.
    built: u64,
    /// The id of the build the last [`Effect::Build`] placed.
    pub last_build: Option<u64>,
    /// What a query's code returned ([`super::Trigger::Query`]).
    pub answer: Option<serde_json::Value>,
    teleports: BTreeMap<String, Teleport>,
    travels: BTreeMap<String, Travel>,
    bodies: BTreeMap<String, Option<Body>>,
    launches: BTreeMap<String, Launch>,
    sky: Option<Option<Sky>>,
    appearances: BTreeMap<String, Option<Appearance>>,
    shows: Vec<Show>,
    signals: Vec<Signal>,
    heard: BTreeMap<String, u64>,
    clock: Option<Option<(f32, u64)>>,
    stores: BTreeMap<(String, String), Option<Value>>,
}

/// A point in a batch to go back to, if what follows fails ([`Batch::rollback`]).
pub struct Mark {
    applied: u32,
    spawned: u32,
    touched: BTreeMap<EntityId, Option<Entity>>,
    counts: BTreeMap<String, i32>,
    items: BTreeMap<(String, String), u32>,
    notes: usize,
    timers: BTreeMap<(String, String), Option<Timer>>,
    effects: usize,
    stats: BTreeMap<(String, String), Stat>,
    hud: BTreeMap<(String, String), Option<Ui>>,
    panels: BTreeMap<String, Option<Panel>>,
    builds: BTreeMap<u64, Option<String>>,
    built: u64,
    teleports: BTreeMap<String, Teleport>,
    travels: BTreeMap<String, Travel>,
    bodies: BTreeMap<String, Option<Body>>,
    launches: BTreeMap<String, Launch>,
    sky: Option<Option<Sky>>,
    appearances: BTreeMap<String, Option<Appearance>>,
    shows: Vec<Show>,
    signals: Vec<Signal>,
    heard: BTreeMap<String, u64>,
    clock: Option<Option<(f32, u64)>>,
    stores: BTreeMap<(String, String), Option<Value>>,
}

/// What a batch changes, to [`GameState::commit`].
pub struct Done {
    touched: BTreeMap<EntityId, Option<Entity>>,
    counts: BTreeMap<String, i32>,
    items: BTreeMap<(String, String), u32>,
    timers: BTreeMap<(String, String), Option<Timer>>,
    stats: BTreeMap<(String, String), Stat>,
    hud: BTreeMap<(String, String), Option<Ui>>,
    panels: BTreeMap<String, Option<Panel>>,
    builds: BTreeMap<u64, Option<String>>,
    teleports: BTreeMap<String, Teleport>,
    travels: BTreeMap<String, Travel>,
    bodies: BTreeMap<String, Option<Body>>,
    launches: BTreeMap<String, Launch>,
    sky: Option<Option<Sky>>,
    appearances: BTreeMap<String, Option<Appearance>>,
    shows: Vec<Show>,
    signals: Vec<Signal>,
    heard: BTreeMap<String, u64>,
    clock: Option<Option<(f32, u64)>>,
    stores: BTreeMap<(String, String), Option<Value>>,
    /// Messages for players: who, and what.
    pub notes: Vec<(String, String)>,
    /// What it does to the world's voxels and portals, in order ([`Effect::Build`],
    /// [`Effect::Unbuild`], [`Effect::Terrain`], [`Effect::Portal`]), with each build's
    /// id: for the world to apply.
    pub world: Vec<(Effect, Option<u64>)>,
}

impl Batch<'_> {
    pub fn get(&self, id: EntityId) -> Option<&Entity> {
        match self.touched.get(&id) {
            Some(e) => e.as_ref(),
            None => self.base.get(id),
        }
    }

    pub fn items(&self, player: &str, item: &str) -> u32 {
        let key = (player.to_string(), item.to_string());
        self.items.get(&key).copied().unwrap_or_else(|| self.base.items(player, item))
    }

    /// Every entity as staged, in id order.
    /// Entities within `radius` meters of `at` (along the ground and up), as staged and
    /// where they are at `now` (walking ones along their way), nearest first.
    pub fn near(&self, at: [f32; 3], radius: f32, now: u64) -> Vec<(EntityId, f32)> {
        let (lo, hi) = ([at[0] - radius, at[2] - radius], [at[0] + radius, at[2] + radius]);
        let mut ids: std::collections::BTreeSet<EntityId> = self.base.within(lo, hi).map(|(id, _)| id).collect();
        ids.extend(self.touched.keys());
        let mut found: Vec<(EntityId, f32)> = ids
            .into_iter()
            .filter_map(|id| {
                let p = super::walk::position_at(self.get(id)?, now as f64);
                let d = ((p[0] - at[0]).powi(2) + (p[1] - at[1]).powi(2) + (p[2] - at[2]).powi(2)).sqrt();
                (d <= radius).then_some((id, d))
            })
            .collect();
        found.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        found
    }

    pub fn ids(&self) -> Vec<EntityId> {
        let base = self.base.entities.keys().filter(|id| !self.touched.contains_key(id));
        let staged = self.touched.iter().filter(|(_, e)| e.is_some()).map(|(id, _)| id);
        let mut ids: Vec<EntityId> = base.chain(staged).copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Every player the world keeps something for, as staged.
    pub fn players(&self) -> Vec<String> {
        let mut out: std::collections::BTreeSet<String> = self.base.players().into_iter().collect();
        out.extend(self.items.keys().map(|(p, _)| p.clone()));
        out.extend(self.stats.keys().map(|(p, _)| p.clone()));
        out.into_iter().collect()
    }

    /// Where the batch is now, to [`Self::rollback`] to.
    pub fn mark(&self) -> Mark {
        Mark {
            applied: self.applied,
            spawned: self.spawned,
            touched: self.touched.clone(),
            counts: self.counts.clone(),
            items: self.items.clone(),
            notes: self.notes.len(),
            timers: self.timers.clone(),
            effects: self.effects.len(),
            stats: self.stats.clone(),
            hud: self.hud.clone(),
            panels: self.panels.clone(),
            builds: self.builds.clone(),
            built: self.built,
            teleports: self.teleports.clone(),
            travels: self.travels.clone(),
            bodies: self.bodies.clone(),
            launches: self.launches.clone(),
            sky: self.sky.clone(),
            appearances: self.appearances.clone(),
            shows: self.shows.clone(),
            signals: self.signals.clone(),
            heard: self.heard.clone(),
            clock: self.clock,
            stores: self.stores.clone(),
        }
    }

    /// Drops everything staged since `mark`.
    pub fn rollback(&mut self, mark: Mark) {
        self.applied = mark.applied;
        self.spawned = mark.spawned;
        self.touched = mark.touched;
        self.counts = mark.counts;
        self.items = mark.items;
        self.notes.truncate(mark.notes);
        self.timers = mark.timers;
        self.effects.truncate(mark.effects);
        self.stats = mark.stats;
        self.hud = mark.hud;
        self.panels = mark.panels;
        self.builds = mark.builds;
        self.built = mark.built;
        self.teleports = mark.teleports;
        self.travels = mark.travels;
        self.bodies = mark.bodies;
        self.launches = mark.launches;
        self.sky = mark.sky;
        self.appearances = mark.appearances;
        self.shows = mark.shows;
        self.signals = mark.signals;
        self.heard = mark.heard;
        self.clock = mark.clock;
        self.stores = mark.stores;
    }

    /// What `player` is called.
    pub fn name(&self, player: &str) -> Option<&str> {
        self.base.name(player)
    }

    /// What `package` keeps under `key`, as staged.
    pub fn recall(&self, package: &str, key: &str) -> Option<Value> {
        match self.stores.get(&(package.to_string(), key.to_string())) {
            Some(staged) => staged.clone(),
            None => self.base.recalled(package, key).cloned(),
        }
    }

    /// One of a player's stats, as staged.
    pub fn stat(&self, player: &str, stat: &str) -> Option<Stat> {
        self.stats.get(&(player.to_string(), stat.to_string())).copied().or_else(|| self.base.stat(player, stat))
    }

    /// The panel a player has open, as staged.
    pub fn panel(&self, player: &str) -> Option<&Panel> {
        match self.panels.get(player) {
            Some(p) => p.as_ref(),
            None => self.base.panel(player),
        }
    }

    /// The package that placed build `id`, as staged.
    fn build_owner(&self, id: u64) -> Option<&str> {
        match self.builds.get(&id) {
            Some(owner) => owner.as_deref(),
            None => self.base.build_owner(id),
        }
    }

    /// `package`'s timer `name` as staged.
    pub fn timer(&self, package: &str, name: &str) -> Option<&Timer> {
        let key = (package.to_string(), name.to_string());
        match self.timers.get(&key) {
            Some(t) => t.as_ref(),
            None => self.base.timers.get(&key),
        }
    }

    /// The player whose action this is.
    pub fn actor(&self) -> &str {
        self.actor
    }

    /// Checks and stages `effect` for the package `grants` describes, or says why not.
    pub fn apply(&mut self, grants: &Grants, effect: &Effect) -> Result<Option<EntityId>, String> {
        let result = self.stage(grants, effect)?;
        self.effects.push(effect.clone());
        Ok(result)
    }

    fn stage(&mut self, grants: &Grants, effect: &Effect) -> Result<Option<EntityId>, String> {
        if self.applied >= self.max_effects {
            return Err(format!("one action may cause at most {} changes here", self.max_effects));
        }
        self.applied += 1;
        let entity = |batch: &Self, id: EntityId| batch.get(id).cloned().ok_or(format!("entity {}.{} doesn't exist", id.change, id.n));
        match effect {
            Effect::Spawn { position, components } => {
                if !grants.spawn {
                    return Err(format!("{} may not spawn entities", grants.package));
                }
                grants.inside(*position)?;
                let count = self.base.count(&grants.package) as i64 + *self.counts.get(&grants.package).unwrap_or(&0) as i64;
                if count + 1 > grants.max_entities as i64 {
                    return Err(format!("{} already has {count} entities; this world allows {}", grants.package, grants.max_entities));
                }
                // A region or the world's rules aren't seen; a pawn is the hero; a decal is drawn.
                let unseen = ["pawn", "region", "world", "decal"].iter().any(|c| components.contains_key(*c));
                if grants.drawn && !components.contains_key("look") && !unseen {
                    return Err(format!(
                        "every piece needs a look: {{model = ...}}, one of the package's models ({})",
                        if grants.models.is_empty() { "it has none yet; add some".to_string() } else { grants.models.join(", ") }
                    ));
                }
                for (name, value) in components {
                    if RESERVED.contains(&name.as_str()) {
                        check_reserved(name, value, grants)?;
                    } else {
                        grants.writable(name)?;
                    }
                    check_value(name, value)?;
                }
                let id = EntityId { change: self.change, n: self.spawned };
                self.spawned += 1;
                *self.counts.entry(grants.package.clone()).or_default() += 1;
                let e = Entity { package: grants.package.clone(), position: snap(*position), components: components.clone() };
                self.touched.insert(id, Some(e));
                Ok(Some(id))
            }
            Effect::Despawn { entity: id } => {
                let e = entity(self, *id)?;
                own(grants, &e)?;
                *self.counts.entry(grants.package.clone()).or_default() -= 1;
                self.touched.insert(*id, None);
                Ok(None)
            }
            Effect::Move { entity: id, position } => {
                let mut e = entity(self, *id)?;
                own(grants, &e)?;
                grants.inside(e.position)?;
                grants.inside(*position)?;
                e.position = snap(*position);
                self.touched.insert(*id, Some(e));
                Ok(None)
            }
            Effect::Set { entity: id, component, value } => {
                let mut e = entity(self, *id)?;
                if RESERVED.contains(&component.as_str()) {
                    own(grants, &e)?;
                    check_reserved(component, value, grants)?;
                } else {
                    grants.writable(component)?;
                }
                grants.inside(e.position)?;
                check_value(component, value)?;
                e.components.insert(component.clone(), value.clone());
                self.touched.insert(*id, Some(e));
                Ok(None)
            }
            Effect::Remove { entity: id, component } => {
                let mut e = entity(self, *id)?;
                if RESERVED.contains(&component.as_str()) {
                    own(grants, &e)?;
                    if component == "look" && grants.drawn && !["pawn", "decal"].iter().any(|c| e.components.contains_key(*c)) {
                        return Err("every piece needs a look; set another one instead of removing it".into());
                    }
                } else {
                    grants.writable(component)?;
                }
                grants.inside(e.position)?;
                e.components.remove(component);
                self.touched.insert(*id, Some(e));
                Ok(None)
            }
            Effect::Give { player, item, count } => {
                grants.writable(item)?;
                let n = self.items(player, item).saturating_add(*count);
                if n > MAX_ITEMS {
                    return Err(format!("nobody can carry more than {MAX_ITEMS} of \"{item}\""));
                }
                self.items.insert((player.clone(), item.clone()), n);
                Ok(None)
            }
            Effect::Take { item, count } => {
                grants.writable(item)?;
                let have = self.items(self.actor, item);
                let n = have.checked_sub(*count).ok_or(format!("you need {count} \"{item}\" and have {have}"))?;
                self.items.insert((self.actor.to_string(), item.clone()), n);
                Ok(None)
            }
            Effect::Notify { player, text } => {
                self.notes.push((player.clone(), text.chars().take(200).collect()));
                Ok(None)
            }
            Effect::Schedule { name, due, every, data, near, feature } => {
                if name.is_empty() || name.chars().count() > 64 {
                    return Err("a timer's name is 1 to 64 characters".into());
                }
                if every.is_some_and(|e| e < MIN_WAIT) {
                    return Err(format!("a timer repeats at most every {MIN_WAIT} s"));
                }
                check_value(name, data)?;
                let key = (grants.package.clone(), name.clone());
                if self.timer(&grants.package, name).is_none() && self.waiting(&grants.package) >= MAX_TIMERS {
                    return Err(format!("{} already has {MAX_TIMERS} timers waiting", grants.package));
                }
                if let Some(id) = near {
                    entity(self, *id)?;
                }
                self.timers.insert(key, Some(Timer { due: *due, every: *every, data: data.clone(), near: *near, feature: feature.clone() }));
                Ok(None)
            }
            Effect::Cancel { name } => {
                self.timers.insert((grants.package.clone(), name.clone()), None);
                Ok(None)
            }
            Effect::SetStat { player, stat, value, max } => {
                grants.writable(stat)?;
                let max = max.or_else(|| self.stat(player, stat).and_then(|s| s.max));
                if !value.is_finite() || max.is_some_and(|m| !m.is_finite() || m <= 0.0) || value.abs() > 1e12 {
                    return Err(format!("\"{stat}\" needs a finite value (and a maximum above 0)"));
                }
                let value = value.max(0.0).min(max.unwrap_or(f64::MAX));
                self.stats.insert((player.clone(), stat.clone()), Stat { value, max });
                Ok(None)
            }
            Effect::Hud { player, id, widget } => {
                if id.is_empty() || id.len() > 40 {
                    return Err("a HUD entry's id is 1 to 40 characters".into());
                }
                if let Some(ui) = widget {
                    ui.check()?;
                    for name in ui.names() {
                        grants.readable(name)?;
                    }
                    let mine = self.base.hud.get(player).map_or(0, |h| h.len());
                    if mine >= MAX_HUD {
                        return Err(format!("a screen holds at most {MAX_HUD} HUD entries"));
                    }
                }
                let key = format!("{}:{id}", grants.package);
                self.hud.insert((player.clone(), key), widget.clone());
                Ok(None)
            }
            Effect::Show { player, panel } => {
                let panel = match panel {
                    Some(spec) => {
                        if spec.title.chars().count() > 60 {
                            return Err("a panel's title is at most 60 characters".into());
                        }
                        spec.body.check()?;
                        for name in spec.body.names() {
                            grants.readable(name)?;
                        }
                        Some(Panel { package: grants.package.clone(), spec: spec.clone() })
                    }
                    None => {
                        if self.panel(player).is_some_and(|p| p.package != grants.package) {
                            return Err("that panel is another package's".into());
                        }
                        None
                    }
                };
                self.panels.insert(player.clone(), panel);
                Ok(None)
            }
            Effect::Build { model, position, turn } => {
                if !grants.build {
                    return Err(format!("{} may not place builds (it needs the build permission)", grants.package));
                }
                if !grants.models.iter().any(|m| m == model) || *turn > 3 {
                    return Err(format!("build places one of the package's models ({}), turned 0-3", grants.models.join(", ")));
                }
                if !position.iter().all(|v| v.is_finite()) {
                    return Err("that isn't a place".into());
                }
                grants.inside(*position)?;
                let id = SCRIPT_BUILDS + self.change * 4096 + self.built;
                self.built += 1;
                self.builds.insert(id, Some(grants.package.clone()));
                self.last_build = Some(id);
                Ok(None)
            }
            Effect::Unbuild { build } => {
                if self.build_owner(*build) != Some(grants.package.as_str()) {
                    return Err(format!("{} may only remove builds it placed", grants.package));
                }
                self.builds.insert(*build, None);
                Ok(None)
            }
            Effect::Teleport { player, position } => {
                if player.is_empty() || !position.iter().all(|v| v.is_finite()) {
                    return Err("teleport takes a player (or \"*\") and a place".into());
                }
                grants.inside(*position)?;
                self.teleports.insert(player.clone(), Teleport { position: *position, change: self.change });
                Ok(None)
            }
            Effect::Travel { player, to } => {
                let world = crate::game::clean_name(to).filter(|name| name.eq_ignore_ascii_case(to.trim()));
                let (Some(world), false) = (world, player.is_empty() || player == "*") else {
                    return Err("travel takes one player and the name of a world of this game".into());
                };
                self.travels.insert(player.clone(), Travel { to: world, change: self.change });
                Ok(None)
            }
            Effect::Portal { build, to } => {
                if !grants.build {
                    return Err(format!("{} may not make portals (it needs the build permission)", grants.package));
                }
                if self.build_owner(*build) != Some(grants.package.as_str()) {
                    return Err(format!("{} may only link builds it placed (build() gives their ids)", grants.package));
                }
                if let Some(to) = to {
                    crate::log::PortalTo::parse(to)?;
                }
                Ok(None)
            }
            Effect::Body { player, body } => {
                if player.is_empty() || player == "*" {
                    return Err("body takes one player".into());
                }
                body.as_ref().map_or(Ok(()), Body::check)?;
                self.bodies.insert(player.clone(), body.filter(|b| *b != Body::default()));
                Ok(None)
            }
            Effect::Launch { player, velocity } => {
                if player.is_empty() || player == "*" || !velocity.iter().all(|v| v.is_finite() && v.abs() <= 40.0) {
                    return Err("launch(player, {x, y, z}): how fast to throw them, up to 40 m/s each way".into());
                }
                self.launches.insert(player.clone(), Launch { velocity: *velocity, change: self.change });
                Ok(None)
            }
            Effect::Play { pos, to, what } => {
                match what {
                    Shown::Effect(fx) => fx.check()?,
                    Shown::Sound(sound) => sound.check()?,
                }
                if !pos.iter().all(|v| v.is_finite()) {
                    return Err("that isn't a place".into());
                }
                if self.shows.len() >= 16 {
                    return Err("at most 16 effects and sounds in one action".into());
                }
                self.shows.push(Show { change: self.change, pos: *pos, to: to.clone(), what: what.clone() });
                Ok(None)
            }
            Effect::Emit { name, data } => {
                let size = serde_json::to_vec(data).map_or(usize::MAX, |b| b.len());
                if name.is_empty() || name.len() > 64 || size > MAX_COMPONENT_BYTES {
                    return Err(format!("emit(name, data): a name up to 64 characters, data up to {MAX_COMPONENT_BYTES} bytes"));
                }
                if self.signals.len() >= 16 {
                    return Err("at most 16 signals in one action".into());
                }
                self.signals.push(Signal { change: self.change, from: grants.package.clone(), name: name.clone(), data: data.clone() });
                Ok(None)
            }
            Effect::Clock { to, at } => {
                if to.is_some_and(|t| !(0.0..=1.0).contains(&t)) {
                    return Err("set_time(0-1): 0 midnight, 0.25 dawn, 0.5 noon, 0.75 dusk".into());
                }
                self.clock = Some(to.map(|t| (t, *at)));
                Ok(None)
            }
            Effect::Heard { upto } => {
                self.heard.insert(grants.package.clone(), *upto);
                Ok(None)
            }
            Effect::Store { key, value } => {
                let size = value.as_ref().map_or(0, |v| serde_json::to_vec(v).map_or(usize::MAX, |b| b.len()));
                if key.is_empty() || key.len() > 64 || size > MAX_COMPONENT_BYTES {
                    return Err(format!("remember(key, value): a key up to 64 characters, a value up to {MAX_COMPONENT_BYTES} bytes"));
                }
                self.stores.insert((grants.package.clone(), key.clone()), value.clone());
                Ok(None)
            }
            Effect::Sky { sky } => {
                sky.as_ref().map_or(Ok(()), Sky::check)?;
                self.sky = Some(sky.clone());
                Ok(None)
            }
            Effect::Appearance { player, appearance } => {
                if let Some(a) = appearance
                    && (!grants.models.contains(&a.model) || !(0.1..=4.0).contains(&a.scale))
                {
                    return Err(format!("appearance(player, {{model = one of {}'s models, scale = 0.1-4}})", grants.package));
                }
                let appearance = appearance.clone().map(|a| Appearance { package: grants.package.clone(), ..a });
                self.appearances.insert(player.clone(), appearance);
                Ok(None)
            }
            Effect::Terrain { area, .. } => {
                if !grants.build {
                    return Err(format!("{} may not reshape land (it needs the build permission)", grants.package));
                }
                let corner = |x: i32, z: i32| [x as f32 * crate::terrain::VOXEL_SIZE, 0.0, z as f32 * crate::terrain::VOXEL_SIZE];
                let [x, z] = area.min;
                let [w, d] = area.size;
                if w <= 0 || d <= 0 {
                    return Err("an area has a size".into());
                }
                for p in [corner(x, z), corner(x + w - 1, z), corner(x, z + d - 1), corner(x + w - 1, z + d - 1)] {
                    grants.inside(p)?;
                }
                Ok(None)
            }
        }
    }

    /// How many timers `package` has waiting, as staged.
    fn waiting(&self, package: &str) -> usize {
        let base = self.base.timers_of(package).filter(|(n, _)| !self.timers.contains_key(&(package.to_string(), n.to_string()))).count();
        base + self.timers.iter().filter(|((p, _), t)| p == package && t.is_some()).count()
    }

    pub fn finish(self) -> Done {
        // The world's part, with the ids builds were given, in order.
        let mut built = 0;
        let change = self.change;
        let world = self
            .effects
            .iter()
            .filter(|e| matches!(e, Effect::Build { .. } | Effect::Unbuild { .. } | Effect::Terrain { .. } | Effect::Portal { .. }))
            .map(|e| {
                let id = matches!(e, Effect::Build { .. }).then(|| {
                    built += 1;
                    SCRIPT_BUILDS + change * 4096 + built - 1
                });
                (e.clone(), id)
            })
            .collect();
        Done {
            touched: self.touched,
            counts: self.counts,
            items: self.items,
            timers: self.timers,
            stats: self.stats,
            hud: self.hud,
            panels: self.panels,
            builds: self.builds,
            teleports: self.teleports,
            travels: self.travels,
            bodies: self.bodies,
            launches: self.launches,
            sky: self.sky,
            appearances: self.appearances,
            shows: self.shows,
            signals: self.signals,
            heard: self.heard,
            clock: self.clock,
            stores: self.stores,
            notes: self.notes,
            world,
        }
    }
}

fn own(grants: &Grants, e: &Entity) -> Result<(), String> {
    if e.package == grants.package {
        Ok(())
    } else {
        Err(format!("{} may only move, remove or restyle its own entities, not {}'s", grants.package, e.package))
    }
}

fn check_value(name: &str, value: &Value) -> Result<(), String> {
    let size = serde_json::to_vec(value).map_or(usize::MAX, |b| b.len());
    if size > MAX_COMPONENT_BYTES {
        return Err(format!("\"{name}\" is {size} bytes; a component may be at most {MAX_COMPONENT_BYTES}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn grants(package: &str, scope: Scope) -> Grants {
        Grants { package: package.into(), write: Vec::new(), spawn: true, scope, max_entities: 2, models: vec!["crate".into()], shaders: Vec::new(), drawn: false, build: false }
    }

    fn spawn(x: f32) -> Effect {
        Effect::Spawn { position: [x, 0.0, 0.0], components: BTreeMap::from([("farm:crop".into(), json!({ "day": 0 }))]) }
    }

    #[test]
    fn batches_are_all_or_nothing() {
        let mut world = GameState::default();
        let farm = grants("farm", Scope::World);
        let mut batch = world.batch(7, "a", 256);
        let id = batch.apply(&farm, &spawn(1.0)).unwrap().unwrap();
        assert_eq!(id, EntityId { change: 7, n: 0 });
        batch.apply(&farm, &Effect::Set { entity: id, component: "farm:crop".into(), value: json!({ "day": 1 }) }).unwrap();
        // Dropping a batch leaves the world as it was.
        drop(batch);
        assert!(world.is_empty());
        let mut batch = world.batch(7, "a", 256);
        batch.apply(&farm, &spawn(1.0)).unwrap();
        batch.apply(&farm, &spawn(2.0)).unwrap();
        assert!(batch.apply(&farm, &spawn(3.0)).unwrap_err().contains("allows 2"));
        let done = batch.finish();
        world.commit(done);
        assert_eq!((world.len(), world.count("farm")), (2, 2));
    }

    #[test]
    fn packages_keep_to_their_namespace_and_scope() {
        let mut world = GameState::default();
        let farm = grants("farm", Scope::Parcels { parcels: vec![[0, 0]] });
        let mut batch = world.batch(1, "a", 256);
        let id = batch.apply(&farm, &spawn(1.0)).unwrap().unwrap();
        // 8 m is a parcel's edge (32 voxels of 25 cm).
        assert!(batch.apply(&farm, &spawn(9.0)).is_err());
        assert!(batch.apply(&farm, &Effect::Move { entity: id, position: [9.0, 0.0, 0.0] }).is_err());
        world.commit(batch.finish());

        let fish = grants("fish", Scope::World);
        let mut batch = world.batch(2, "a", 256);
        let set = |component: &str| Effect::Set { entity: id, component: component.into(), value: json!(true) };
        assert!(batch.apply(&fish, &set("farm:crop")).unwrap_err().contains("belongs to farm"));
        assert!(batch.apply(&fish, &Effect::Despawn { entity: id }).is_err());
        // Its own components on someone else's entity are fine (a "fished here" mark).
        batch.apply(&fish, &set("fish:visited")).unwrap();
        let mut fish = fish.clone();
        fish.write.push("farm".into());
        batch.apply(&fish, &set("farm:crop")).unwrap();
        assert!(batch.apply(&fish, &set("farm:nope!")).is_err());
        assert!(batch.apply(&fish, &Effect::Set { entity: id, component: "farm:big".into(), value: json!("x".repeat(5000)) }).is_err());
    }

    #[test]
    fn items_are_given_and_taken_from_the_actor() {
        let mut world = GameState::default();
        let farm = grants("farm", Scope::World);
        let give = |player: &str, count| Effect::Give { player: player.into(), item: "farm:seed".into(), count };
        let take = |count| Effect::Take { item: "farm:seed".into(), count };
        let mut batch = world.batch(1, "a", 256);
        batch.apply(&farm, &give("a", 3)).unwrap();
        batch.apply(&farm, &take(2)).unwrap();
        assert!(batch.apply(&farm, &take(2)).unwrap_err().contains("have 1"));
        assert!(batch.apply(&farm, &Effect::Give { player: "a".into(), item: "fish:cod".into(), count: 1 }).is_err());
        assert!(batch.apply(&farm, &give("a", MAX_ITEMS)).is_err());
        world.commit(batch.finish());
        assert_eq!(world.items("a", "farm:seed"), 1);

        // "b" acting takes from "b", not from "a".
        let mut batch = world.batch(2, "b", 256);
        assert!(batch.apply(&farm, &take(1)).is_err());
        batch.apply(&farm, &give("b", 1)).unwrap();
        batch.apply(&farm, &take(1)).unwrap();
        world.commit(batch.finish());
        assert_eq!((world.items("a", "farm:seed"), world.items("b", "farm:seed")), (1, 0));
        assert_eq!(world.inventory("b").count(), 0);
    }

    #[test]
    fn timers_go_off_once_per_span() {
        let once = Timer { due: 10, every: None, data: Value::Null, near: None, feature: None };
        assert_eq!((once.fires(5, 9), once.fires(5, 10), once.fires(10, 20)), (None, Some((10, 1)), Some((10, 1))));
        let every = Timer { due: 10, every: Some(3), data: Value::Null, near: None, feature: None };
        // 10, 13, 16, 19: by 20, all four, folded into the last.
        assert_eq!(every.fires(0, 20), Some((19, 4)));
        assert_eq!(every.fires(10, 15), Some((13, 1)));
        assert_eq!(every.fires(13, 15), None);
        assert_eq!((every.next(0), every.next(10), every.next(14)), (Some(10), Some(13), Some(16)));
        let world = GameState::default();
        let mut batch = world.batch(1, "a", 1000);
        let game = grants("game", Scope::World);
        let schedule = |name: String| Effect::Schedule { name, due: 5, every: None, data: Value::Null, near: None, feature: None };
        for n in 0..MAX_TIMERS {
            batch.apply(&game, &schedule(n.to_string())).unwrap();
        }
        assert!(batch.apply(&game, &schedule("more".into())).unwrap_err().contains("timers waiting"));
        // Replacing one is fine.
        batch.apply(&game, &schedule("0".into())).unwrap();
        let fast = Effect::Schedule { name: "0".into(), due: 5, every: Some(0), data: Value::Null, near: None, feature: None };
        assert!(batch.apply(&game, &fast).is_err());
    }

    #[test]
    fn engine_components_are_checked_and_owned() {
        let mut world = GameState::default();
        let game = grants("game", Scope::World);
        let look = json!({ "model": "crate" });
        let mut batch = world.batch(1, "a", 3);
        let spawn = Effect::Spawn { position: [0.0; 3], components: BTreeMap::from([("look".into(), look), ("label".into(), json!("Box"))]) };
        let id = batch.apply(&game, &spawn).unwrap().unwrap();
        let bad = Effect::Set { entity: id, component: "look".into(), value: json!({ "model": "barrel" }) };
        assert!(batch.apply(&game, &bad).unwrap_err().contains("one of the package's models (crate)"));
        // Another package can't restyle it, even with permission to write game's data.
        let mut other = grants("other", Scope::World);
        other.write.push("game".into());
        let label = Effect::Set { entity: id, component: "label".into(), value: json!("Mine") };
        assert!(batch.apply(&other, &label).is_err());
        // The batch caps how much one action does.
        assert!(batch.apply(&game, &label).unwrap_err().contains("at most 3"));
        assert_eq!(batch.ids(), vec![id]);
        world.commit(batch.finish());
        assert_eq!(world.get(id).unwrap().components["label"], "Box");
    }
}
