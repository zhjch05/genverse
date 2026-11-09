//! What a package is made of besides its scripts, and how agents write and change it.
//!
//! A package's definitions name its **items** (things players carry), with a title for
//! what players see and, optionally, one of its models as their icon:
//!
//! ```json
//! "farm:turnip": { "kind": "item", "title": "Turnip", "model": "turnip" }
//! ```
//!
//! Everything else a game does (crops, shops, spawners, villagers) its scripts do: the
//! engine keeps only what scripts can't do themselves. Definitions of kinds from before
//! (crop, source, fixture, shop, spawner, npc) are ignored, so old worlds still load.
//!
//! Agents write packages as a [`GameSpec`], and change a world's package with one: what
//! a change sends replaces what's there by name ([`GameSpec::onto`]).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{Manifest, Package, Permission, package::split_name};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Def {
    /// Something players carry.
    Item {
        title: String,
        /// One of the package's models, drawn as its icon (and held in the hand).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
}

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

/// Kinds of definition from before scripts did their jobs: left alone.
const RETIRED: [&str; 6] = ["crop", "source", "fixture", "shop", "spawner", "npc"];

/// What's wrong with the package's definitions, phrased for the agent that wrote them.
/// Definitions outside its namespace are left to [`Package::check`].
pub fn check(package: &Package) -> Vec<String> {
    let ns = &package.manifest.name;
    let mut errors = Vec::new();
    for (name, value) in package.defs.iter().filter(|(name, _)| split_name(name).is_some_and(|(n, _)| n == ns)) {
        if value["kind"].as_str().is_some_and(|k| RETIRED.contains(&k)) {
            continue;
        }
        match serde_json::from_value::<Def>(value.clone()) {
            Err(e) => errors.push(format!("\"{name}\": {e} (the only kind is item; scripts do the rest)")),
            Ok(Def::Item { title, model }) => {
                if title.trim().is_empty() || title.len() > 80 {
                    errors.push(format!("\"{name}\": title must be 1-80 characters"));
                }
                if let Some(model) = model.filter(|m| !package.models.contains_key(m)) {
                    errors.push(format!("\"{name}\": model \"{model}\" must be one of the package's models"));
                }
            }
        }
    }
    errors.sort();
    errors.dedup();
    errors
}

/// A package as an agent writes it: names local to the package (`turnip`), every field
/// present and null where unused, so it fits a strict JSON schema
/// (`build_spec.schema.json`'s `game`). Also a change to one ([`Self::onto`]).
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

/// A voxel model the package's entities (and items) can look like.
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
    /// item: one of the package's models, as its icon.
    #[serde(default)]
    pub model: Option<String>,
}

impl GameSpec {
    /// The package it describes, as version `version`. Mistakes come out as definitions
    /// [`check`] explains.
    pub fn to_package(&self, version: u32) -> Package {
        let ns = &self.name;
        let full = |local: &str| if local.contains(':') { local.to_string() } else { format!("{ns}:{local}") };
        let defs = self
            .defs
            .iter()
            .map(|d| {
                let mut value = json!({ "kind": d.kind, "title": d.title });
                if let Some(model) = &d.model {
                    value["model"] = model.clone().into();
                }
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
        let defs = defs(package)
            .into_iter()
            .map(|(name, Def::Item { title, model })| DefSpec { name: local(name), kind: "item".into(), title, model })
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

/// An item's title, for what players see ("Turnip"); its name if it has none.
pub fn title(package: &Package, name: &str) -> String {
    match defs(package).get(name) {
        Some(Def::Item { title, .. }) => title.clone(),
        None => name.rsplit(':').next().unwrap_or(name).replace('_', " "),
    }
}

/// An item's icon: one of its package's models, if it has one.
pub fn icon(package: &Package, name: &str) -> Option<String> {
    match defs(package).get(name) {
        Some(Def::Item { model, .. }) => model.clone(),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logic::package::tests::farm;

    #[test]
    fn items_check_and_round_trip() {
        let package = farm(1);
        assert!(check(&package).is_empty(), "{:?}", check(&package));
        assert_eq!(title(&package, "farm:turnip"), "Turnip");
        assert_eq!(title(&package, "farm:pumpkin"), "pumpkin");
        let spec = GameSpec::from_package(&package);
        assert_eq!(defs(&spec.to_package(1)), defs(&package));
        let json = serde_json::to_string(&spec).unwrap();
        assert!(json.contains("\"model\":null"), "every field is present for the strict schema");
    }

    #[test]
    fn check_explains_bad_defs_and_leaves_old_kinds_alone() {
        let mut bad = farm(1);
        bad.defs.insert("farm:rock".into(), json!({ "kind": "boulder" }));
        bad.defs.insert("farm:gem".into(), json!({ "kind": "item", "title": "Gem", "model": "nope" }));
        bad.defs.insert("farm:old_crop".into(), json!({ "kind": "crop", "title": "Turnips", "seed": "farm:seed" }));
        let errors = check(&bad);
        assert_eq!(errors.len(), 2, "{errors:#?}");
        assert!(errors.iter().any(|e| e.contains("farm:rock") && e.contains("only kind is item")));
        assert!(errors.iter().any(|e| e.contains("farm:gem") && e.contains("models")));
    }
}
