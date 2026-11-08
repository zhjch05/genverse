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

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    Batch, Effect, Event, Grants, Intent, Manifest, Package, Permission, Runtime,
    package::split_name,
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
}

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
                errors.push(format!("\"{name}\": {e} (kinds are item, crop, source and fixture)"));
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
            Def::Item { title } | Def::Crop { title, .. } | Def::Source { title, .. } | Def::Fixture { title }
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
                (full(&d.name), value)
            })
            .collect();
        let manifest =
            Manifest { name: ns.clone(), version, title: self.title.clone(), permissions: vec![Permission::Spawn], depends: Vec::new() };
        let scripts = self.scripts.iter().map(|s| (s.name.clone(), s.source.clone())).collect();
        Package { manifest, defs, scripts }
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
                }
            })
            .collect();
        let scripts = package.scripts.iter().map(|(name, source)| ScriptSpec { name: name.clone(), source: source.clone() }).collect();
        Self { name: ns.into(), title: package.manifest.title.clone(), defs, scripts }
    }
}

/// A definition's title, for what players see ("Turnip seeds"); the name if it has none.
pub fn title(package: &Package, name: &str) -> String {
    match defs(package).get(name) {
        Some(Def::Item { title } | Def::Crop { title, .. } | Def::Source { title, .. } | Def::Fixture { title }) => title.clone(),
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
        for effect in self.answer(package, event, world)? {
            world.apply(grants, &effect)?;
        }
        Ok(())
    }
}

impl DataRules {
    fn answer(&self, package: &Package, event: &Event, state: &Batch) -> Result<Vec<Effect>, String> {
        let defs = defs(package);
        let today = event.time.day;
        match event.intent {
            Intent::Place { item, position } => {
                if let Some(Def::Source { .. }) = defs.get(item.as_str()) {
                    let components = [(item.clone(), json!({}))].into();
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
            Intent::UseItem { .. } | Intent::Key { .. } => Ok(Vec::new()),
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
