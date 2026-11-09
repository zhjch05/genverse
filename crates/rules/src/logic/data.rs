//! Built-in rules for packages that are only data. A package's definitions say what its
//! items, crops and sources are, and [`DataRules`] plays them, so a farming loop needs
//! no script at all:
//!
//! ```json
//! "farm:turnip_seed": { "kind": "item", "title": "Turnip seeds" },
//! "farm:turnip":      { "kind": "item", "title": "Turnip" },
//! "farm:turnip_crop": { "kind": "crop", "title": "Turnips", "seed": "farm:turnip_seed",
//!                       "stage_days": [1, 2, 1], "harvest": { "item": "farm:turnip", "count": 2 } },
//! "farm:seed_bin":    { "kind": "source", "title": "Seed bin",
//!                       "gives": { "item": "farm:turnip_seed", "count": 5 } }
//! ```
//!
//! Placing a seed plants its crop (and uses up the seed); a crop is ripe once its stages'
//! days have passed, and interacting with a ripe one harvests it. A source is placed
//! by its own name and gives each player its items once every `every_days`.
//!
//! Living things need no script either ([`super::walk`]): a **spawner** keeps up to
//! `max` pieces around it (one every `every_seconds`, by day, night or always), and an
//! **npc** keeps a daily routine of stops, relative to where it's put down:
//!
//! ```json
//! "farm:slime_pool": { "kind": "spawner", "title": "Slime pool", "model": "slime",
//!                      "spawns": "Slime", "max": 4, "every_seconds": 30, "radius": 8,
//!                      "when": "night", "wander": true },
//! "farm:baker":      { "kind": "npc", "title": "Baker", "model": "baker", "radius": 3,
//!                      "routine": [{ "at": "7:00", "to": [0, 0] }, { "at": "18:00", "to": [12, 4] }] }
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    Batch, Effect, Event, Grants, Intent, Manifest, Package, Permission, Runtime, Trigger,
    package::split_name,
    walk::{self, MAX_SPAWNED, MAX_STOPS, MAX_WALK},
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Def {
    Item {
        title: String,
    },
    Crop {
        title: String,
        /// The item that plants it.
        seed: String,
        /// How many days each growth stage takes.
        stage_days: Vec<u32>,
        harvest: Stack,
    },
    /// Something placed in the world that hands out items, like a seed bin or a tree.
    Source {
        title: String,
        gives: Stack,
        #[serde(default = "one")]
        every_days: u32,
    },
    /// Something players can put down by its name, which the package's script sets up
    /// (`on_place`): a game board, a machine.
    Fixture { title: String },
    /// A shop: what it sells, for what. Put down (or spawned by a script with a `shop`
    /// component), E opens it, and buying needs no script.
    Shop { title: String, sells: Vec<Offer> },
    /// Keeps up to `max` pieces around where it's put down: one every `every_seconds`,
    /// within `radius` m, `when` it's "day", "night" or "any" time. They look like
    /// `model`, are labeled `spawns`, carry the component `tag` (the def's own name
    /// unless given) for scripts to answer, and wander unless `wander` is false.
    Spawner {
        title: String,
        model: String,
        spawns: String,
        max: u32,
        every_seconds: u32,
        radius: f32,
        #[serde(default = "any")]
        when: String,
        #[serde(default = "yes")]
        wander: bool,
    },
    /// A character put down by name: looks like `model`, keeps its `routine` (times of
    /// day and places, in meters from where it was put down), and wanders `radius` m
    /// around each stop if given.
    Npc {
        title: String,
        model: String,
        #[serde(default)]
        routine: Vec<Stop>,
        #[serde(default)]
        radius: Option<f32>,
    },
}

/// One stop of an NPC's day.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stop {
    /// "8:30".
    pub at: String,
    /// Meters east and south of where the NPC was put down.
    pub to: [f32; 2],
}

fn any() -> String {
    "any".into()
}

fn yes() -> bool {
    true
}

/// One thing a shop sells, and its price.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Offer {
    pub item: Stack,
    pub price: Stack,
}

/// The most a shop sells.
pub const MAX_OFFERS: usize = 10;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stack {
    pub item: String,
    #[serde(default = "one")]
    pub count: u32,
}

fn one() -> u32 {
    1
}

const MAX_STACK: u32 = 999;
const MAX_STAGES: usize = 16;

/// The package's own definitions that parse, by name.
pub fn defs(package: &Package) -> BTreeMap<&str, Def> {
    let ns = package.manifest.name.as_str();
    package
        .defs
        .iter()
        .filter(|(name, _)| split_name(name).is_some_and(|(n, _)| n == ns))
        .filter_map(|(name, value)| Some((name.as_str(), serde_json::from_value(value.clone()).ok()?)))
        .collect()
}

/// What's wrong with the package's definitions, phrased for the agent that wrote them.
/// Definitions outside its namespace are left to [`Package::check`].
pub fn check(package: &Package) -> Vec<String> {
    let ns = &package.manifest.name;
    let mut errors = Vec::new();
    let own = package.defs.iter().filter(|(name, _)| split_name(name).is_some_and(|(n, _)| n == ns));
    let parsed: BTreeMap<&str, Def> = own
        .filter_map(|(name, value)| match serde_json::from_value(value.clone()) {
            Ok(def) => Some((name.as_str(), def)),
            Err(e) => {
                errors.push(format!("\"{name}\": {e} (kinds are item, crop, source, fixture, shop, spawner and npc)"));
                None
            }
        })
        .collect();
    let item = |errors: &mut Vec<String>, name: &str, what: &str, of: &str| {
        if !matches!(parsed.get(of), Some(Def::Item { .. })) {
            errors.push(format!("\"{name}\": its {what} \"{of}\" must be an item this package defines"));
        }
    };
    let stack = |errors: &mut Vec<String>, name: &str, what: &str, s: &Stack| {
        item(errors, name, what, &s.item);
        if !(1..=MAX_STACK).contains(&s.count) {
            errors.push(format!("\"{name}\": {what} count must be 1 to {MAX_STACK}"));
        }
    };
    for (name, def) in &parsed {
        match def {
            Def::Item { title }
            | Def::Crop { title, .. }
            | Def::Source { title, .. }
            | Def::Fixture { title }
            | Def::Shop { title, .. }
            | Def::Spawner { title, .. }
            | Def::Npc { title, .. }
                if title.trim().is_empty() || title.len() > 80 =>
            {
                errors.push(format!("\"{name}\": title must be 1-80 characters"));
            }
            _ => {}
        }
        match def {
            Def::Item { .. } => {}
            Def::Fixture { .. } if package.scripts.is_empty() => {
                errors.push(format!("\"{name}\": a fixture needs a script with on_place to set it up"));
            }
            Def::Fixture { .. } => {}
            Def::Crop { seed, stage_days, harvest, .. } => {
                item(&mut errors, name, "seed", seed);
                stack(&mut errors, name, "harvest", harvest);
                if stage_days.is_empty() || stage_days.len() > MAX_STAGES || stage_days.iter().any(|d| !(1..=365).contains(d)) {
                    errors.push(format!("\"{name}\": stage_days must be 1 to {MAX_STAGES} stages of 1 to 365 days"));
                }
                let planted_by = parsed.iter().filter(|(_, d)| matches!(d, Def::Crop { seed: s, .. } if s == seed)).count();
                if planted_by > 1 {
                    errors.push(format!("\"{name}\": another crop is planted with \"{seed}\" too"));
                }
            }
            Def::Source { gives, every_days, .. } => {
                stack(&mut errors, name, "gives", gives);
                if !(1..=365).contains(every_days) {
                    errors.push(format!("\"{name}\": every_days must be 1 to 365"));
                }
            }
            Def::Spawner { model, max, every_seconds, radius, when, spawns, .. } => {
                if !package.models.contains_key(model) {
                    errors.push(format!("\"{name}\": model \"{model}\" must be one of the package's models"));
                }
                if !(1..=MAX_SPAWNED as u32).contains(max) || !(5..=86_400).contains(every_seconds) || !(1.0..=32.0).contains(radius) {
                    errors.push(format!("\"{name}\": max is 1-{MAX_SPAWNED}, every_seconds 5-86400, radius 1-32 m"));
                }
                if !matches!(when.as_str(), "any" | "day" | "night") {
                    errors.push(format!("\"{name}\": when is \"any\", \"day\" or \"night\""));
                }
                if spawns.chars().count() > 60 {
                    errors.push(format!("\"{name}\": spawns (their label) is at most 60 characters"));
                }
            }
            Def::Npc { model, routine, radius, .. } => {
                if !package.models.contains_key(model) {
                    errors.push(format!("\"{name}\": model \"{model}\" must be one of the package's models"));
                }
                if routine.len() > MAX_STOPS {
                    errors.push(format!("\"{name}\": a routine has at most {MAX_STOPS} stops"));
                }
                for stop in routine {
                    if walk::time_of_day(&stop.at).is_none() || stop.to.iter().any(|v| !v.is_finite() || v.abs() > MAX_WALK) {
                        errors.push(format!("\"{name}\": each stop is {{at: \"8:30\", to: [x, z]}}, within {MAX_WALK} m"));
                    }
                }
                if radius.is_some_and(|r| !(1.0..=32.0).contains(&r)) {
                    errors.push(format!("\"{name}\": radius (how far it wanders) is 1-32 m"));
                }
            }
            Def::Shop { sells, .. } => {
                if sells.is_empty() || sells.len() > MAX_OFFERS {
                    errors.push(format!("\"{name}\": a shop sells 1 to {MAX_OFFERS} things"));
                }
                for offer in sells {
                    stack(&mut errors, name, "sells", &offer.item);
                    stack(&mut errors, name, "price", &offer.price);
                }
            }
        }
    }
    errors.sort();
    errors.dedup();
    errors
}

/// A data package as an agent writes it: a flat list with names local to the package
/// (`turnip_seed`), every field present and null where unused, so it fits a strict
/// JSON schema (`build_spec.schema.json`'s `game`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GameSpec {
    pub name: String,
    pub title: String,
    pub defs: Vec<DefSpec>,
    #[serde(default)]
    pub scripts: Vec<ScriptSpec>,
    #[serde(default)]
    pub models: Vec<ModelSpec>,
    /// Names of defs, script files and models a change takes out ([`Self::onto`]).
    #[serde(default)]
    pub remove: Option<Vec<String>>,
    /// Script files a change renames: their pieces and timers follow.
    #[serde(default)]
    pub rename: Option<Vec<Rename>>,
}

/// A script file's new name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rename {
    pub from: String,
    pub to: String,
}

/// A voxel model the package's entities can look like.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelSpec {
    pub name: String,
    pub build: crate::build::BuildSpec,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScriptSpec {
    /// A file name ending in `.luau`.
    pub name: String,
    pub source: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DefSpec {
    pub name: String,
    pub kind: String,
    pub title: String,
    pub seed: Option<String>,
    pub stage_days: Option<Vec<u32>>,
    pub harvest: Option<Stack>,
    pub gives: Option<Stack>,
    pub every_days: Option<u32>,
    /// shop: what it sells, for what.
    #[serde(default)]
    pub sells: Option<Vec<Offer>>,
    /// spawner, npc: the model it (or what it spawns) looks like.
    #[serde(default)]
    pub model: Option<String>,
    /// spawner: the label of what it spawns.
    #[serde(default)]
    pub spawns: Option<String>,
    /// spawner: how many it keeps around.
    #[serde(default)]
    pub max: Option<u32>,
    /// spawner: seconds between spawns.
    #[serde(default)]
    pub every_seconds: Option<u32>,
    /// spawner: how far around it they appear and wander; npc: how far it wanders.
    #[serde(default)]
    pub radius: Option<f32>,
    /// spawner: "day", "night" or "any".
    #[serde(default)]
    pub when: Option<String>,
    /// spawner: whether what it spawns wanders (null: yes).
    #[serde(default)]
    pub wander: Option<bool>,
    /// npc: where to be when.
    #[serde(default)]
    pub routine: Option<Vec<Stop>>,
}

impl GameSpec {
    /// The package it describes, as version `version`. Mistakes (a crop without a seed)
    /// come out as definitions [`check`] explains.
    pub fn to_package(&self, version: u32) -> Package {
        let ns = &self.name;
        let full = |local: &str| if local.contains(':') { local.to_string() } else { format!("{ns}:{local}") };
        let stack = |s: &Option<Stack>| s.as_ref().map(|s| json!({ "item": full(&s.item), "count": s.count }));
        let defs = self
            .defs
            .iter()
            .map(|d| {
                let mut value = json!({ "kind": d.kind, "title": d.title });
                let mut field = |key: &str, v: Option<serde_json::Value>| {
                    if let Some(v) = v {
                        value[key] = v;
                    }
                };
                field("seed", d.seed.as_deref().map(|s| full(s).into()));
                field("stage_days", d.stage_days.as_ref().map(|days| json!(days)));
                field("harvest", stack(&d.harvest));
                field("gives", stack(&d.gives));
                field("every_days", d.every_days.map(Into::into));
                field("model", d.model.clone().map(Into::into));
                field("spawns", d.spawns.clone().map(Into::into));
                field("max", d.max.map(Into::into));
                field("every_seconds", d.every_seconds.map(Into::into));
                field("radius", d.radius.map(Into::into));
                field("when", d.when.clone().map(Into::into));
                field("wander", d.wander.map(Into::into));
                field("routine", d.routine.as_ref().map(|r| json!(r)));
                field(
                    "sells",
                    d.sells.as_ref().map(|offers| {
                        offers
                            .iter()
                            .map(|o| json!({ "item": { "item": full(&o.item.item), "count": o.item.count }, "price": { "item": full(&o.price.item), "count": o.price.count } }))
                            .collect()
                    }),
                );
                (full(&d.name), value)
            })
            .collect();
        // Agents' packages may place builds and reshape land, within the installer's budget.
        let permissions = vec![Permission::Spawn, Permission::Build];
        let renamed = self.rename.iter().flatten().map(|r| (r.from.clone(), r.to.clone())).collect();
        let manifest = Manifest { name: ns.clone(), version, title: self.title.clone(), permissions, depends: Vec::new(), renamed };
        let scripts = self.scripts.iter().map(|s| (s.name.clone(), s.source.clone())).collect();
        let models = self.models.iter().map(|m| (m.name.clone(), m.build.clone())).collect();
        Package { manifest, defs, scripts, models }
    }

    /// The other way, to show an agent what's installed so it can change it.
    pub fn from_package(package: &Package) -> Self {
        let ns = package.manifest.name.as_str();
        let local = |name: &str| name.strip_prefix(ns).and_then(|n| n.strip_prefix(':')).unwrap_or(name).to_string();
        let stack = |s: &Stack| Stack { item: local(&s.item), count: s.count };
        let defs = defs(package)
            .into_iter()
            .map(|(name, def)| {
                let name = local(name);
                match def {
                    Def::Item { title } => DefSpec { name, kind: "item".into(), title, ..Default::default() },
                    Def::Crop { title, seed, stage_days, harvest } => DefSpec {
                        name,
                        kind: "crop".into(),
                        title,
                        seed: Some(local(&seed)),
                        stage_days: Some(stage_days),
                        harvest: Some(stack(&harvest)),
                        ..Default::default()
                    },
                    Def::Source { title, gives, every_days } => DefSpec {
                        name,
                        kind: "source".into(),
                        title,
                        gives: Some(stack(&gives)),
                        every_days: Some(every_days),
                        ..Default::default()
                    },
                    Def::Fixture { title } => DefSpec { name, kind: "fixture".into(), title, ..Default::default() },
                    Def::Shop { title, sells } => DefSpec {
                        name,
                        kind: "shop".into(),
                        title,
                        sells: Some(sells.iter().map(|o| Offer { item: stack(&o.item), price: stack(&o.price) }).collect()),
                        ..Default::default()
                    },
                    Def::Spawner { title, model, spawns, max, every_seconds, radius, when, wander } => DefSpec {
                        name,
                        kind: "spawner".into(),
                        title,
                        model: Some(model),
                        spawns: Some(spawns),
                        max: Some(max),
                        every_seconds: Some(every_seconds),
                        radius: Some(radius),
                        when: Some(when),
                        wander: Some(wander),
                        ..Default::default()
                    },
                    Def::Npc { title, model, routine, radius } => DefSpec {
                        name,
                        kind: "npc".into(),
                        title,
                        model: Some(model),
                        radius,
                        routine: Some(routine),
                        ..Default::default()
                    },
                }
            })
            .collect();
        let scripts = package.scripts.iter().map(|(name, source)| ScriptSpec { name: name.clone(), source: source.clone() }).collect();
        let models = package.models.iter().map(|(name, build)| ModelSpec { name: name.clone(), build: build.clone() }).collect();
        Self { name: ns.into(), title: package.manifest.title.clone(), defs, scripts, models, remove: None, rename: None }
    }

    /// This change on top of `base` (what the world runs under the same name, if
    /// anything): what it sends replaces what's there by name, what it lists in `remove`
    /// goes, and the rest stays. So each request adds to the world's game logic.
    pub fn onto(&self, base: Option<&GameSpec>) -> GameSpec {
        let Some(base) = base else {
            // Nothing to keep a title from: the name will do.
            let title = if self.title.trim().is_empty() { self.name.clone() } else { self.title.clone() };
            return GameSpec { remove: None, rename: None, title, ..self.clone() };
        };
        let gone = |name: &str| self.remove.iter().flatten().any(|r| r == name);
        let mut out = base.clone();
        out.remove = None;
        // A renamed file keeps its source unless the change sends it anew.
        out.rename = self.rename.clone();
        for r in self.rename.iter().flatten() {
            if let Some(old) = out.scripts.iter_mut().find(|s| s.name == r.from) {
                old.name = r.to.clone();
            }
        }
        if !self.title.trim().is_empty() {
            out.title = self.title.clone();
        }
        out.defs.retain(|d| !gone(&d.name) && !self.defs.iter().any(|n| n.name == d.name));
        out.defs.extend(self.defs.iter().cloned());
        out.scripts.retain(|s| !gone(&s.name) && !self.scripts.iter().any(|n| n.name == s.name));
        out.scripts.extend(self.scripts.iter().cloned());
        out.scripts.sort_by(|a, b| a.name.cmp(&b.name));
        out.models.retain(|m| !gone(&m.name) && !self.models.iter().any(|n| n.name == m.name));
        out.models.extend(self.models.iter().cloned());
        out
    }

    /// What this change does to `base`, in a few words: "added wolves.luau, changed
    /// merchant.luau, removed torch".
    pub fn change_summary(&self, base: Option<&GameSpec>) -> String {
        let mut added = Vec::new();
        let mut changed = Vec::new();
        let had = |name: &str| {
            base.is_some_and(|b| b.defs.iter().any(|d| d.name == name) || b.scripts.iter().any(|s| s.name == name) || b.models.iter().any(|m| m.name == name))
        };
        let names = self.scripts.iter().map(|s| &s.name).chain(self.defs.iter().map(|d| &d.name)).chain(self.models.iter().map(|m| &m.name));
        for name in names {
            if had(name) { changed.push(name.clone()) } else { added.push(name.clone()) }
        }
        let removed: Vec<String> = self.remove.iter().flatten().filter(|n| had(n)).cloned().collect();
        let mut parts: Vec<String> = self.rename.iter().flatten().map(|r| format!("renamed {} to {}", r.from, r.to)).collect();
        for (what, list) in [("added", added), ("changed", changed), ("removed", removed)] {
            if !list.is_empty() {
                parts.push(format!("{what} {}", list.join(", ")));
            }
        }
        if parts.is_empty() { "no changes".into() } else { parts.join("; ") }
    }
}

/// A definition's title, for what players see ("Turnip seeds"); the name if it has none.
pub fn title(package: &Package, name: &str) -> String {
    match defs(package).get(name) {
        Some(
            Def::Item { title }
            | Def::Crop { title, .. }
            | Def::Source { title, .. }
            | Def::Fixture { title }
            | Def::Shop { title, .. }
            | Def::Spawner { title, .. }
            | Def::Npc { title, .. },
        ) => title.clone(),
        None => name.to_string(),
    }
}

/// Which growth stage a crop planted on day `planted` is in on `today`, and whether
/// it's ripe (past its last stage).
pub fn crop_stage(stage_days: &[u32], planted: u64, today: u64) -> (usize, bool) {
    let mut grown = today.saturating_sub(planted);
    for (stage, days) in stage_days.iter().enumerate() {
        if grown < *days as u64 {
            return (stage, false);
        }
        grown -= *days as u64;
    }
    (stage_days.len(), true)
}

/// Plays packages' definitions. The default [`Runtime`]: worlds with only data
/// packages need nothing else.
pub struct DataRules;

impl Runtime for DataRules {
    fn handle(&self, package: &Package, event: &Event, world: &mut Batch, grants: &Grants) -> Result<(), String> {
        if place_living(package, event, world, grants)? {
            return Ok(());
        }
        for effect in self.answer(package, event, world)? {
            world.apply(grants, &effect)?;
        }
        Ok(())
    }
}

/// Puts down a spawner or an NPC the event places, if it does, scripts or not: the
/// engine runs them. Returns whether it did.
pub fn place_living(package: &Package, event: &Event, world: &mut Batch, grants: &Grants) -> Result<bool, String> {
    let Trigger::Intent(Intent::Place { item, position }) = event.trigger else { return Ok(false) };
    let defs = defs(package);
    let (value, components) = match defs.get(item.as_str()) {
        Some(Def::Spawner { title, model, spawns, max, every_seconds, radius, when, wander }) => {
            let spawner = json!({ "model": model, "label": spawns, "tag": item, "max": max, "every": every_seconds,
                "radius": radius, "when": when, "wander": wander });
            (("spawner", Some(spawner)), [("label".to_string(), json!(title))].into())
        }
        Some(Def::Npc { title, model, routine, radius }) => {
            let at = |to: [f32; 2]| [position[0] + to[0], position[1], position[2] + to[1]];
            let mut components: BTreeMap<String, serde_json::Value> =
                [("label".to_string(), json!(title)), ("look".to_string(), json!({ "model": model })), (item.clone(), json!({}))].into();
            if let Some(radius) = radius {
                components.insert("wander".into(), json!({ "home": position, "radius": radius, "speed": 1.0, "pause": 6 }));
            }
            let stops: Vec<_> = routine.iter().filter_map(|s| Some(json!({ "at": walk::time_of_day(&s.at)?, "to": at(s.to) }))).collect();
            let start = if stops.is_empty() { ("wander", radius.map(|_| components["wander"].clone())) } else { ("routine", Some(json!(stops))) };
            (start, components)
        }
        _ => return Ok(false),
    };
    let ((name, value), at) = (value, event.at);
    // Spawned with its engine component, so a spawner needs no look of its own.
    let mut components = components;
    if let Some(value) = &value {
        components.insert(name.to_string(), value.clone());
    }
    let id = world.apply(grants, &Effect::Spawn { position: *position, components })?.ok_or("nothing spawned")?;
    if let Some(value) = value {
        for effect in walk::start_effects(id, name, Some(value), at) {
            world.apply(grants, &effect)?;
        }
    }
    Ok(true)
}

impl DataRules {
    fn answer(&self, package: &Package, event: &Event, state: &Batch) -> Result<Vec<Effect>, String> {
        let defs = defs(package);
        let today = event.time.day;
        let Trigger::Intent(intent) = event.trigger else { return Ok(Vec::new()) };
        match intent {
            Intent::Place { item, position } => {
                if let Some(Def::Source { .. }) = defs.get(item.as_str()) {
                    let components = [(item.clone(), json!({}))].into();
                    return Ok(vec![Effect::Spawn { position: *position, components }]);
                }
                if let Some(Def::Shop { title, .. }) = defs.get(item.as_str()) {
                    let components = [("shop".to_string(), json!(item)), ("label".to_string(), json!(title))].into();
                    return Ok(vec![Effect::Spawn { position: *position, components }]);
                }
                let crop = defs.iter().find(|(_, d)| matches!(d, Def::Crop { seed, .. } if seed == item));
                Ok(match crop {
                    Some((name, _)) => vec![
                        Effect::Take { item: item.clone(), count: 1 },
                        Effect::Spawn { position: *position, components: [(name.to_string(), json!({ "planted": today }))].into() },
                    ],
                    None => Vec::new(),
                })
            }
            Intent::Choose { .. } => Ok(Vec::new()),
            Intent::Interact { entity: id } => {
                let Some(entity) = state.get(*id).filter(|e| e.package == package.manifest.name) else {
                    return Ok(Vec::new());
                };
                for (component, value) in &entity.components {
                    match defs.get(component.as_str()) {
                        Some(Def::Crop { title, stage_days, harvest, .. }) => {
                            let planted = value["planted"].as_u64().unwrap_or(today);
                            let (stage, ripe) = crop_stage(stage_days, planted, today);
                            if !ripe {
                                return Err(format!("the {title} aren't ripe yet (stage {} of {})", stage + 1, stage_days.len()));
                            }
                            let give = Effect::Give { player: event.player.into(), item: harvest.item.clone(), count: harvest.count };
                            return Ok(vec![Effect::Despawn { entity: *id }, give]);
                        }
                        Some(Def::Source { title, gives, every_days }) => {
                            let last = value["taken"][event.player].as_u64();
                            if last.is_some_and(|day| today < day + *every_days as u64) {
                                return Err(format!("you've already taken from the {title}; come back another day"));
                            }
                            let mut value = value.clone();
                            if !value["taken"].is_object() {
                                value["taken"] = json!({});
                            }
                            value["taken"][event.player] = today.into();
                            return Ok(vec![
                                Effect::Give { player: event.player.into(), item: gives.item.clone(), count: gives.count },
                                Effect::Set { entity: *id, component: component.clone(), value },
                            ]);
                        }
                        _ => {}
                    }
                }
                Ok(Vec::new())
            }
            Intent::UseItem { .. } | Intent::Key { .. } | Intent::Hit { .. } => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::logic::package::tests::farm;

    /// A small turnip farm: a seed bin, seeds, and turnips that ripen over four days.
    pub fn sample_farm() -> GameSpec {
        let item = |name: &str, title: &str| DefSpec { name: name.into(), kind: "item".into(), title: title.into(), ..Default::default() };
        let stack = |item: &str, count| Some(Stack { item: item.into(), count });
        GameSpec {
            scripts: Vec::new(),
            models: Vec::new(),
            remove: None, rename: None,
            name: "farm".into(),
            title: "Turnip farm".into(),
            defs: vec![
                item("turnip_seed", "Turnip seeds"),
                item("turnip", "Turnip"),
                DefSpec {
                    name: "turnip_crop".into(),
                    kind: "crop".into(),
                    title: "Turnips".into(),
                    seed: Some("turnip_seed".into()),
                    stage_days: Some(vec![1, 2, 1]),
                    harvest: stack("turnip", 2),
                    ..Default::default()
                },
                DefSpec { name: "seed_bin".into(), kind: "source".into(), title: "Seed bin".into(), gives: stack("turnip_seed", 5), ..Default::default() },
            ],
        }
    }

    #[test]
    fn stages() {
        let days = [1, 2, 1];
        assert_eq!(crop_stage(&days, 10, 10), (0, false));
        assert_eq!(crop_stage(&days, 10, 12), (1, false));
        assert_eq!(crop_stage(&days, 10, 13), (2, false));
        assert_eq!(crop_stage(&days, 10, 14), (3, true));
    }

    #[test]
    fn the_sample_farm_checks() {
        let package = sample_farm().to_package(1);
        assert!(package.check().is_ok(), "{:?}", package.check());
        assert_eq!(title(&package, "farm:seed_bin"), "Seed bin");
    }

    #[test]
    fn game_specs_round_trip() {
        let spec = GameSpec::from_package(&farm(1));
        assert_eq!(spec.defs.len(), 4);
        assert!(spec.defs.iter().any(|d| d.seed.as_deref() == Some("turnip_seed")));
        let back = spec.to_package(1);
        assert_eq!(defs(&back), defs(&farm(1)));
        assert!(back.check().is_ok(), "{:?}", back.check());
        let json = serde_json::to_string(&spec).unwrap();
        assert!(json.contains("\"every_days\":null"), "every field is present for the strict schema");
    }

    #[test]
    fn check_explains_bad_defs() {
        assert!(check(&farm(1)).is_empty(), "{:?}", check(&farm(1)));
        let mut bad = farm(1);
        bad.defs.insert("farm:rock".into(), json!({ "kind": "boulder" }));
        bad.defs.insert("farm:weed".into(), json!({ "kind": "crop", "title": "Weeds", "seed": "farm:turnip_crop",
            "stage_days": [], "harvest": { "item": "fish:cod" } }));
        bad.defs.insert("farm:extra".into(), json!({ "kind": "item", "title": "x", "colour": "red" }));
        let errors = check(&bad);
        assert_eq!(errors.len(), 5, "{errors:#?}");
        assert!(errors.iter().any(|e| e.contains("farm:rock") && e.contains("kinds are")));
        assert!(errors.iter().any(|e| e.contains("farm:extra") && e.contains("colour")));
    }
}
