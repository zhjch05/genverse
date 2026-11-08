//! Game logic: what the world *does*, next to what it looks like.
//!
//! Agents write [`Package`]s (content as data, and later scripts), and a world installs
//! them, everywhere or on some parcels. Players act through [`Intent`]s ("interact with
//! that"), never by sending state; the installed packages' rules answer with
//! [`Effect`]s, and those land only if every one is something its package may do. All
//! of it goes through the log, so replay rebuilds the game's state like it rebuilds
//! the voxels. See `docs/game-logic.md`.

pub mod clock;
pub mod entity;
pub mod package;

use std::{collections::BTreeMap, sync::Arc};

use bevy_math::{IVec2, Vec2};
use serde::{Deserialize, Serialize};

pub use clock::{ClockRules, WorldTime};
pub use entity::{Effect, Entities, Entity, EntityId, Grants};
pub use package::{Manifest, Package, PackageRef, Permission};

use crate::{land::parcel_of, log::fnv1a, terrain::VOXEL_SIZE};

/// Who may install and change packages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogicMode {
    /// Anyone, on land they may change, with the default permissions.
    #[default]
    Open,
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
}

impl Default for LogicRules {
    fn default() -> Self {
        Self { mode: LogicMode::Open, clock: ClockRules::default(), max_entities: 10_000, max_effects: 256 }
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
                let column = (Vec2::new(position[0], position[2]) / VOXEL_SIZE).floor().as_ivec2();
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
                return Err(format!("a package goes on 1 to {MAX_SCOPE_PARCELS} parcels"));
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
    Interact { entity: EntityId },
    UseItem { item: String, target: EntityId },
    /// Puts an item down somewhere (plants a seed, sets a trap): how entities begin.
    Place { item: String, position: [f32; 3] },
}

impl Intent {
    /// Where it happens, which decides the packages asked.
    pub fn position(&self, entities: &Entities) -> Result<[f32; 3], String> {
        match self {
            Intent::Interact { entity } | Intent::UseItem { target: entity, .. } => entities
                .get(*entity)
                .map(|e| e.position)
                .ok_or(format!("entity {}.{} doesn't exist", entity.change, entity.n)),
            Intent::Place { position, .. } if position.iter().all(|v| v.is_finite()) => Ok(*position),
            Intent::Place { .. } => Err("that isn't a place".into()),
        }
    }
}

/// What a package's rules are asked about.
#[derive(Clone, Debug)]
pub struct Event<'a> {
    pub change: u64,
    pub player: &'a str,
    pub intent: &'a Intent,
    pub time: WorldTime,
    /// For the package's random numbers: the same for everyone replaying this change.
    pub seed: u64,
}

/// Runs packages' rules. It must answer the same way for the same inputs on every
/// machine, since replay asks it again.
pub trait Runtime: Send + Sync {
    /// What `package` does about `event`, seeing the world as it was before the change.
    fn handle(&self, package: &Package, event: &Event, entities: &Entities) -> Result<Vec<Effect>, String>;
}

/// An installed package.
#[derive(Clone, Debug, PartialEq)]
pub struct Install {
    pub package: PackageRef,
    pub scope: Scope,
    /// Who installed it, and answers for what it does.
    pub by: String,
}

/// The game's state: packages, what's installed where, and the entities.
#[derive(Clone, Default)]
pub struct Logic {
    /// Every package the log has, by hash.
    pub packages: BTreeMap<String, Arc<Package>>,
    /// By package name.
    pub installed: BTreeMap<String, Install>,
    pub entities: Entities,
    /// Without one, intents are kept but nothing answers them.
    pub runtime: Option<Arc<dyn Runtime>>,
}

impl Logic {
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
        let found = self.packages.get(&package.hash).ok_or(format!("package {} wasn't uploaded", package.name))?;
        if found.manifest.name != package.name || found.manifest.version != package.version {
            return Err(format!("package {} doesn't match its hash", package.name));
        }
        found.check().map_err(|e| e.join("; "))?;
        scope.check()?;
        if !admin && (*scope == Scope::World || found.needs_admin()) {
            return Err(format!(
                "only an admin can install {} {}",
                package.name,
                if *scope == Scope::World { "everywhere" } else { "with permission to change other packages" }
            ));
        }
        if let Some(old) = self.installed.get(&package.name) {
            if old.by != player && !admin {
                return Err(format!("{} was installed by someone else", package.name));
            }
            if old.package == *package && old.scope == *scope {
                return Err(format!("{} v{} is already installed there", package.name, package.version));
            }
        }
        if let Some(dep) = found.manifest.depends.iter().find(|d| !self.installed.contains_key(*d)) {
            return Err(format!("{} needs {dep} installed first", package.name));
        }
        self.installed.insert(package.name.clone(), Install { package: package.clone(), scope: scope.clone(), by: player.into() });
        Ok(())
    }

    /// Uninstalls `name`. Its entities stay, dormant, for if it comes back.
    pub fn uninstall(&mut self, player: &str, admin: bool, rules: &LogicRules, name: &str) -> Result<(), String> {
        allowed(admin, rules)?;
        let install = self.installed.get(name).ok_or(format!("{name} isn't installed"))?;
        if install.by != player && !admin {
            return Err(format!("{name} was installed by someone else"));
        }
        let needed = self.installed.values().find(|i| self.package(i).is_some_and(|p| p.manifest.depends.iter().any(|d| d == name)));
        if let Some(other) = needed {
            return Err(format!("{} needs {name}", other.package.name));
        }
        self.installed.remove(name);
        Ok(())
    }

    /// Asks every package installed where the intent's target is, in name order, and
    /// applies all their effects, or none if any isn't allowed.
    pub fn act(&mut self, change: u64, player: &str, at: u64, world_seed: u32, rules: &LogicRules, intent: &Intent) -> Result<(), String> {
        let position = intent.position(&self.entities)?;
        let Some(runtime) = self.runtime.clone() else {
            return Ok(());
        };
        let mut batch = self.entities.batch(change);
        let mut effects = 0;
        for install in self.installed.values().filter(|i| i.scope.contains(position)) {
            let package = self.package(install).ok_or(format!("package {} is missing", install.package.name))?;
            let seed = fnv1a(format!("{world_seed}/{change}/{}", install.package.name).as_bytes());
            let event = Event { change, player, intent, time: rules.clock.at(at), seed };
            let answer = runtime.handle(package, &event, &self.entities).map_err(|e| format!("{}: {e}", install.package.name))?;
            effects += answer.len();
            if effects > rules.max_effects as usize {
                return Err(format!("that would cause more than {} effects", rules.max_effects));
            }
            let grants = grants(package, install, rules);
            for effect in &answer {
                batch.apply(&grants, effect)?;
            }
        }
        let done = batch.finish();
        self.entities.commit(done);
        Ok(())
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
    }
}
