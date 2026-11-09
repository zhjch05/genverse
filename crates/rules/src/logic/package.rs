//! Packages: game content and rules an agent writes, which a world installs.
//!
//! A package owns a namespace (its name): every definition and component it makes is
//! named `name:thing`. It's identified by the hash of its contents, so the log can name
//! it in a few bytes and replay still gets exactly what was installed.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::build::{BuildLimits, BuildSpec, compile};

/// The most a package may weigh, as JSON.
pub const MAX_PACKAGE_BYTES: usize = 512 * 1024;
pub const MAX_DEFS: usize = 2000;
pub const MAX_SCRIPTS: usize = 64;
pub const MAX_MODELS: usize = 64;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Package {
    pub manifest: Manifest,
    /// Content definitions by name ("farm:turnip"): see [`super::data::Def`].
    #[serde(default)]
    pub defs: BTreeMap<String, serde_json::Value>,
    /// Script sources by file name ("main.luau"): see [`super::script`].
    #[serde(default)]
    pub scripts: BTreeMap<String, String>,
    /// Voxel models by local name ("crate"), in the same format and under the same
    /// limits as builds, so everything in a world shares one style. Entities show one
    /// with their `look`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, BuildSpec>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Also the namespace it owns.
    pub name: String,
    pub version: u32,
    pub title: String,
    /// What it may do beyond its own namespace.
    #[serde(default)]
    pub permissions: Vec<Permission>,
    /// Packages that must be installed first.
    #[serde(default)]
    pub depends: Vec<String>,
    /// Script files this version renamed, old name to new: their pieces and timers
    /// follow ([`super::Upgrade`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub renamed: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Permission {
    /// Read another package's components.
    Read { namespace: String },
    /// Change another package's components. Needs an admin to install.
    Write { namespace: String },
    /// Create entities, and remove its own, inside where it's installed.
    Spawn,
    /// Place its voxel models as builds and reshape land, inside where it's installed,
    /// charged to the installer's budget and land.
    Build,
}

impl Permission {
    pub fn needs_admin(&self) -> bool {
        matches!(self, Permission::Write { .. })
    }
}

/// How the log names a package.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageRef {
    pub name: String,
    pub version: u32,
    /// [`Package::hash`].
    pub hash: String,
}

/// Whether `name` can be a namespace: 2 to 32 lowercase letters, digits and `_`,
/// starting with a letter. `core` is the kernel's.
pub fn valid_namespace(name: &str) -> bool {
    (2..=32).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && name != "core"
}

/// Splits `farm:turnip` into its namespace and local name, if it's well formed.
pub fn split_name(name: &str) -> Option<(&str, &str)> {
    let (ns, local) = name.split_once(':')?;
    let ok = valid_namespace(ns)
        && (1..=64).contains(&local.len())
        && local.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.');
    ok.then_some((ns, local))
}

impl Package {
    /// SHA-256 of its JSON, in hex.
    pub fn hash(&self) -> String {
        let bytes = serde_json::to_vec(self).unwrap_or_default();
        Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn reference(&self) -> PackageRef {
        PackageRef { name: self.manifest.name.clone(), version: self.manifest.version, hash: self.hash() }
    }

    /// Everything wrong with it, phrased for the agent that wrote it.
    pub fn check(&self) -> Result<(), Vec<String>> {
        let m = &self.manifest;
        let mut errors = Vec::new();
        if !valid_namespace(&m.name) {
            errors.push(format!(
                "package name \"{}\" must be 2-32 lowercase letters, digits or _, starting with a letter (and not \"core\")",
                m.name
            ));
        }
        if m.version == 0 {
            errors.push("version starts at 1".into());
        }
        if m.title.trim().is_empty() || m.title.len() > 80 {
            errors.push("title must be 1-80 characters".into());
        }
        let size = serde_json::to_vec(self).map_or(usize::MAX, |b| b.len());
        if size > MAX_PACKAGE_BYTES {
            errors.push(format!("package is {size} bytes; the most is {MAX_PACKAGE_BYTES}"));
        }
        if self.defs.len() > MAX_DEFS {
            errors.push(format!("{} definitions; the most is {MAX_DEFS}", self.defs.len()));
        }
        for name in self.defs.keys() {
            match split_name(name) {
                Some((ns, _)) if ns == m.name => {}
                _ => errors.push(format!("definition \"{name}\" must be named \"{}:<lowercase name>\"", m.name)),
            }
        }
        errors.extend(super::data::check(self));
        errors.extend(super::script::check(self));
        if self.models.len() > MAX_MODELS {
            errors.push(format!("{} models; the most is {MAX_MODELS}", self.models.len()));
        }
        for (name, spec) in &self.models {
            if split_name(&format!("{}:{name}", m.name)).is_none() || name.contains(':') {
                errors.push(format!("model \"{name}\" needs a lowercase name (letters, digits, _)"));
            }
            if let Err(problems) = compile(spec, &BuildLimits::default()) {
                errors.push(format!("model \"{name}\": {}", problems.join("; ")));
            }
        }
        if self.scripts.len() > MAX_SCRIPTS {
            errors.push(format!("{} scripts; the most is {MAX_SCRIPTS}", self.scripts.len()));
        }
        for file in self.scripts.keys() {
            let stem = file.strip_suffix(".luau").unwrap_or_default();
            if stem.is_empty() || !stem.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                errors.push(format!("script \"{file}\" must be a plain file name ending in .luau"));
            }
        }
        for p in &m.permissions {
            if let Permission::Read { namespace } | Permission::Write { namespace } = p
                && (!valid_namespace(namespace) || *namespace == m.name)
            {
                errors.push(format!("permission names \"{namespace}\", which isn't another package's namespace"));
            }
        }
        for dep in &m.depends {
            if !valid_namespace(dep) || *dep == m.name {
                errors.push(format!("dependency \"{dep}\" isn't another package's name"));
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(errors) }
    }

    /// Whether installing it takes an admin, wherever it goes.
    pub fn needs_admin(&self) -> bool {
        self.manifest.permissions.iter().any(Permission::needs_admin)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn farm(version: u32) -> Package {
        Package {
            manifest: Manifest {
                name: "farm".into(),
                version,
                title: "Farming".into(),
                permissions: vec![Permission::Spawn],
                depends: Vec::new(), renamed: Default::default(),
            },
            models: BTreeMap::new(),
            defs: BTreeMap::from([
                ("farm:turnip_seed".into(), serde_json::json!({ "kind": "item", "title": "Turnip seeds" })),
                ("farm:turnip".into(), serde_json::json!({ "kind": "item", "title": "Turnip" })),
            ]),
            scripts: BTreeMap::new(),
        }
    }

    #[test]
    fn names_and_hashes() {
        assert!(valid_namespace("farm_2") && !valid_namespace("Farm") && !valid_namespace("core"));
        assert_eq!(split_name("farm:turnip"), Some(("farm", "turnip")));
        assert_eq!(split_name("farm:"), None);
        let (a, b) = (farm(1), farm(2));
        assert_eq!(a.hash(), a.clone().hash());
        assert_ne!(a.hash(), b.hash());
        let back: Package = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        assert_eq!(back.hash(), a.hash());
    }

    #[test]
    fn check_catches_foreign_names() {
        assert!(farm(1).check().is_ok());
        let mut bad = farm(0);
        bad.defs.insert("fish:cod".into(), serde_json::json!({}));
        bad.scripts.insert("../x.luau".into(), String::new());
        bad.manifest.permissions.push(Permission::Write { namespace: "farm".into() });
        assert_eq!(bad.check().unwrap_err().len(), 4);
    }
}
