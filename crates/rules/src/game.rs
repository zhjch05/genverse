//! A game: what's hosted or joined, holding many worlds (`docs/worlds.md`). Its own
//! small log says which worlds exist, who runs the game, the saves made of all of them
//! at once, and the game's packages: game rules installed once for the whole game, which
//! each world turns on or off. Each world keeps its own log ([`crate::log::WorldLog`]);
//! a game's admins are every world's admins.

use std::{collections::BTreeMap, sync::Arc};

use serde::{Deserialize, Serialize};

use crate::{
    log::{Provenance, WORLD},
    logic::{Install, Package, Scope},
};

/// A game or world name as kept on disk: lowercase letters, digits, - and _.
pub fn clean_name(name: &str) -> Option<String> {
    let name: String = name.trim().to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    (!name.is_empty() && name.len() <= 40 && name != "trash").then_some(name)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GameAction {
    /// A new world, with its ground.
    CreateWorld { name: String, seed: u32, flat: bool },
    /// Takes a world out of the game (its files move to the trash; nothing is deleted).
    RemoveWorld { name: String },
    /// Who runs the game, and every world in it.
    SetAdmins { admins: Vec<String> },
    /// A save of every world at once: each world's last change then.
    Save { name: String, worlds: BTreeMap<String, u64> },
    /// Adds a package to the game, or a new version of one: worlds that run it move to
    /// this version when they're next loaded (at once, on a server, for loaded ones).
    AddPackage { package: Box<Package> },
    /// Takes a package out of the game: worlds that run it stop.
    RemovePackage { name: String },
    /// Loads a game save: each world goes back to it. `signed` is the admin's signature
    /// of each world's [`crate::log::Action::Restore`], by world.
    LoadSave { name: String, signed: BTreeMap<String, String> },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GameChange {
    pub id: u64,
    pub by: Provenance,
    pub action: GameAction,
}

/// What a world does to match its game's packages ([`GameLog::sync`]).
#[derive(Clone, Debug)]
pub enum Sync {
    /// Moves to the game's version of a package it runs.
    Upgrade(Arc<Package>, Scope),
    /// Stops running a package the game took out.
    Remove(String),
    /// The game should take on this package the world runs.
    Adopt(String),
}

/// A world's ground, as the game made it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ground {
    pub seed: u32,
    pub flat: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GameLog {
    pub name: String,
    #[serde(default)]
    pub changes: Vec<GameChange>,
}

impl GameLog {
    pub fn new(name: &str) -> Self {
        Self { name: name.into(), changes: Vec::new() }
    }

    /// The worlds in the game, in the order they were made.
    pub fn worlds(&self) -> Vec<(String, Ground)> {
        let mut out: Vec<(String, Ground)> = Vec::new();
        for c in &self.changes {
            match &c.action {
                GameAction::CreateWorld { name, seed, flat } => {
                    out.retain(|(n, _)| n != name);
                    out.push((name.clone(), Ground { seed: *seed, flat: *flat }));
                }
                GameAction::RemoveWorld { name } => out.retain(|(n, _)| n != name),
                _ => {}
            }
        }
        out
    }

    pub fn world(&self, name: &str) -> Option<Ground> {
        self.worlds().into_iter().find(|(n, _)| n == name).map(|(_, g)| g)
    }

    /// Where players start: the first world still in the game.
    pub fn start(&self) -> Option<String> {
        self.worlds().into_iter().next().map(|(n, _)| n)
    }

    pub fn admins(&self) -> Vec<String> {
        self.changes
            .iter()
            .rev()
            .find_map(|c| match &c.action {
                GameAction::SetAdmins { admins } => Some(admins.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    pub fn is_admin(&self, player: &str) -> bool {
        self.admins().iter().any(|a| a == player)
    }

    /// Game saves, newest last: the change, its name, and each world's last change.
    pub fn saves(&self) -> impl DoubleEndedIterator<Item = (&GameChange, &str, &BTreeMap<String, u64>)> {
        self.changes.iter().filter_map(|c| match &c.action {
            GameAction::Save { name, worlds } => Some((c, name.as_str(), worlds)),
            _ => None,
        })
    }

    /// The game's packages, by name: the newest version of each one still in it.
    pub fn packages(&self) -> BTreeMap<String, Arc<Package>> {
        let mut out = BTreeMap::new();
        for c in &self.changes {
            match &c.action {
                GameAction::AddPackage { package } => {
                    out.insert(package.manifest.name.clone(), Arc::new((**package).clone()));
                }
                GameAction::RemovePackage { name } => {
                    out.remove(name);
                }
                _ => {}
            }
        }
        out
    }

    /// Whether the game took the package `name` out.
    pub fn removed(&self, name: &str) -> bool {
        self.changes.iter().rev().find_map(|c| match &c.action {
            GameAction::AddPackage { package } if package.manifest.name == name => Some(false),
            GameAction::RemovePackage { name: n } if n == name => Some(true),
            _ => None,
        }) == Some(true)
    }

    /// The save named `name`, newest first.
    pub fn save_named(&self, name: &str) -> Option<&BTreeMap<String, u64>> {
        self.saves().rev().find(|(_, n, _)| n.eq_ignore_ascii_case(name)).map(|(_, _, w)| w)
    }

    /// What a world running `installed` must do to match the game's packages.
    pub fn sync(&self, installed: &BTreeMap<String, Install>) -> Vec<Sync> {
        let catalog = self.packages();
        let mut out = Vec::new();
        for (name, install) in installed {
            match catalog.get(name) {
                Some(p) if p.hash() != install.package.hash => out.push(Sync::Upgrade(p.clone(), install.scope.clone())),
                Some(_) => {}
                None if self.removed(name) => out.push(Sync::Remove(name.clone())),
                // Installed before the game kept packages: the game takes it on.
                None => out.push(Sync::Adopt(name.clone())),
            }
        }
        out
    }

    /// Whether `player` may do `action` now.
    pub fn check(&self, player: &str, action: &GameAction) -> Result<(), String> {
        // A new game's first player runs it; the server acts for the game.
        let admin = self.is_admin(player) || self.admins().is_empty() || player == WORLD;
        match action {
            GameAction::CreateWorld { name, .. } => {
                if !admin {
                    return Err("only the game's admins make worlds".into());
                }
                if clean_name(name).as_deref() != Some(name.as_str()) {
                    return Err("a world's name is lowercase letters, digits, - and _".into());
                }
                if self.world(name).is_some() {
                    return Err(format!("there's already a world called \"{name}\""));
                }
            }
            GameAction::RemoveWorld { name } => {
                if !admin {
                    return Err("only the game's admins remove worlds".into());
                }
                if self.world(name).is_none() {
                    return Err(format!("there's no world \"{name}\""));
                }
                if self.worlds().len() == 1 {
                    return Err("a game keeps at least one world".into());
                }
            }
            GameAction::SetAdmins { admins } => {
                if !admin {
                    return Err("only admins change who runs the game".into());
                }
                if admins.is_empty() {
                    return Err("a game needs an admin".into());
                }
            }
            GameAction::Save { name, .. } => {
                if name.trim().is_empty() || name.chars().count() > 40 {
                    return Err("a save's name is 1 to 40 characters".into());
                }
            }
            GameAction::AddPackage { package } => {
                if !admin {
                    return Err("only the game's admins change its rules".into());
                }
                package.check().map_err(|e| e.join("; "))?;
            }
            GameAction::RemovePackage { name } => {
                if !admin {
                    return Err("only the game's admins change its rules".into());
                }
                if !self.packages().contains_key(name) {
                    return Err(format!("the game has no package {name}"));
                }
            }
            GameAction::LoadSave { name, .. } => {
                if !admin {
                    return Err("only the game's admins load saves".into());
                }
                if self.save_named(name).is_none() {
                    return Err(format!("there's no game save \"{name}\""));
                }
            }
        }
        Ok(())
    }

    /// Checks `action` and appends it. Returns its id.
    pub fn push(&mut self, by: Provenance, action: GameAction) -> Result<u64, String> {
        self.check(&by.player, &action)?;
        let id = self.changes.last().map_or(1, |c| c.id + 1);
        self.changes.push(GameChange { id, by, action });
        Ok(id)
    }
}

/// What a player signs to change a game: who, how, when, and what.
pub fn signing_bytes(by: &Provenance, action: &GameAction) -> Vec<u8> {
    let by = Provenance { prompt: None, ..by.clone() };
    serde_json::to_vec(&("game", by, action)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn by(player: &str) -> Provenance {
        Provenance::new(player, player, None, None, 0)
    }

    #[test]
    fn games_hold_worlds_and_admins() {
        let mut game = GameLog::new("local");
        // The first player runs a new game.
        game.push(by("ada"), GameAction::SetAdmins { admins: vec!["ada".into()] }).unwrap();
        game.push(by("ada"), GameAction::CreateWorld { name: "home".into(), seed: 1, flat: false }).unwrap();
        game.push(by("ada"), GameAction::CreateWorld { name: "sokoban".into(), seed: 1, flat: true }).unwrap();
        assert!(game.push(by("bo"), GameAction::CreateWorld { name: "mine".into(), seed: 1, flat: true }).is_err());
        assert!(game.push(by("ada"), GameAction::CreateWorld { name: "home".into(), seed: 2, flat: true }).is_err());
        assert!(game.push(by("ada"), GameAction::CreateWorld { name: "Bad Name".into(), seed: 2, flat: true }).is_err());
        assert_eq!(game.start().as_deref(), Some("home"));
        assert_eq!(game.world("sokoban"), Some(Ground { seed: 1, flat: true }));
        game.push(by("ada"), GameAction::RemoveWorld { name: "home".into() }).unwrap();
        assert!(game.push(by("ada"), GameAction::RemoveWorld { name: "sokoban".into() }).unwrap_err().contains("at least one"));
        assert_eq!(game.start().as_deref(), Some("sokoban"));
        // Anyone saves the game.
        let worlds = BTreeMap::from([("sokoban".to_string(), 7)]);
        game.push(by("bo"), GameAction::Save { name: "before".into(), worlds }).unwrap();
        assert_eq!(game.saves().count(), 1);
    }

    #[test]
    fn worlds_follow_the_games_packages() {
        use crate::logic::package::tests::farm;
        let mut game = GameLog::new("g");
        game.push(by("ada"), GameAction::SetAdmins { admins: vec!["ada".into()] }).unwrap();
        let install = |version| Install { package: farm(version).reference(), scope: Scope::World, by: "ada".into() };
        let installed = |version| BTreeMap::from([("farm".to_string(), install(version))]);
        // A world running a package the game doesn't know yet: the game takes it on.
        assert!(matches!(game.sync(&installed(1)).as_slice(), [Sync::Adopt(n)] if n == "farm"));
        assert!(game.push(by("bo"), GameAction::AddPackage { package: Box::new(farm(1)) }).is_err());
        game.push(by("ada"), GameAction::AddPackage { package: Box::new(farm(1)) }).unwrap();
        assert!(game.sync(&installed(1)).is_empty());
        // A new version: worlds running the old one move to it.
        game.push(by("ada"), GameAction::AddPackage { package: Box::new(farm(2)) }).unwrap();
        assert!(matches!(game.sync(&installed(1)).as_slice(), [Sync::Upgrade(p, _)] if p.manifest.version == 2));
        // Taken out of the game: worlds stop running it.
        game.push(by("ada"), GameAction::RemovePackage { name: "farm".into() }).unwrap();
        assert!(matches!(game.sync(&installed(2)).as_slice(), [Sync::Remove(n)] if n == "farm"));
        assert!(game.packages().is_empty());
    }
}
