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
    let name: String = name
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    (!name.is_empty() && name.len() <= 40 && name != "trash").then_some(name)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GameAction {
    /// A new world, with its ground.
    CreateWorld {
        name: String,
        seed: u32,
        flat: bool,
        /// Its landscape; left out while it's the usual one.
        #[serde(default, skip_serializing_if = "crate::recipe::Recipe::is_default")]
        recipe: crate::recipe::Recipe,
    },
    /// Takes a world out of the game (its files move to the trash; nothing is deleted).
    RemoveWorld { name: String },
    /// Gives a world a new name. Its log, its place in saves and the portals that name
    /// it go with it ([`GameLog::current`]).
    RenameWorld { from: String, to: String },
    /// Who runs the game, and every world in it.
    SetAdmins { admins: Vec<String> },
    /// A save of every world at once: each world's last change then.
    Save {
        name: String,
        worlds: BTreeMap<String, u64>,
    },
    /// Adds a package to the game, or a new version of one: worlds that run it move to
    /// this version when they're next loaded (at once, on a server, for loaded ones).
    AddPackage { package: Box<Package> },
    /// Takes a package out of the game: worlds that run it stop.
    RemovePackage { name: String },
    /// Loads a game save: each world goes back to it. `signed` is the admin's signature
    /// of each world's [`load_action`], by world.
    LoadSave {
        name: String,
        signed: BTreeMap<String, String>,
    },
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
    /// Its landscape ([`crate::recipe`]).
    pub recipe: crate::recipe::Recipe,
}

impl Ground {
    /// A new world's log on this ground.
    pub fn log(&self) -> crate::log::WorldLog {
        crate::log::WorldLog {
            flat: self.flat,
            recipe: self.recipe,
            ..crate::log::WorldLog::new(self.seed)
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GameLog {
    pub name: String,
    #[serde(default)]
    pub changes: Vec<GameChange>,
}

impl GameLog {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            changes: Vec::new(),
        }
    }

    /// The worlds in the game, in the order they were made.
    pub fn worlds(&self) -> Vec<(String, Ground)> {
        let mut out: Vec<(String, Ground)> = Vec::new();
        for c in &self.changes {
            match &c.action {
                GameAction::CreateWorld {
                    name,
                    seed,
                    flat,
                    recipe,
                } => {
                    out.retain(|(n, _)| n != name);
                    out.push((
                        name.clone(),
                        Ground {
                            seed: *seed,
                            flat: *flat,
                            recipe: *recipe,
                        },
                    ));
                }
                GameAction::RemoveWorld { name } => out.retain(|(n, _)| n != name),
                GameAction::RenameWorld { from, to } => {
                    if let Some(world) = out.iter_mut().find(|(n, _)| n == from) {
                        world.0 = to.clone();
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// What the world once called `name` is called now: itself while a world has that
    /// name, else where its renames led (portals and saves keep the old name). `None`
    /// if it's gone.
    pub fn current(&self, name: &str) -> Option<String> {
        if self.world(name).is_some() {
            return Some(name.to_string());
        }
        self.renamed_after(name, 0)
            .filter(|now| self.world(now).is_some())
    }

    /// What the world called `name` as of change `id` is called now, following the
    /// renames after it; `None` if it was taken out since.
    fn renamed_after(&self, name: &str, id: u64) -> Option<String> {
        let mut now = name.to_string();
        for c in self.changes.iter().filter(|c| c.id > id) {
            match &c.action {
                GameAction::RenameWorld { from, to } if *from == now => now = to.clone(),
                GameAction::RemoveWorld { name } if *name == now => return None,
                _ => {}
            }
        }
        Some(now)
    }

    pub fn world(&self, name: &str) -> Option<Ground> {
        self.worlds()
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, g)| g)
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

    /// Game saves, newest last: the change, its name, and each world's last change, by
    /// the world's name now (worlds taken out since are left out).
    pub fn saves(
        &self,
    ) -> impl DoubleEndedIterator<Item = (&GameChange, &str, BTreeMap<String, u64>)> {
        self.changes.iter().filter_map(|c| match &c.action {
            GameAction::Save { name, worlds } => {
                let worlds = worlds
                    .iter()
                    .filter_map(|(w, last)| Some((self.renamed_after(w, c.id)?, *last)))
                    .collect();
                Some((c, name.as_str(), worlds))
            }
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
    pub fn save_named(&self, name: &str) -> Option<BTreeMap<String, u64>> {
        self.saves()
            .rev()
            .find(|(_, n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, _, w)| w)
    }

    /// What a world running `installed` must do to match the game's packages.
    pub fn sync(&self, installed: &BTreeMap<String, Install>) -> Vec<Sync> {
        let catalog = self.packages();
        let mut out = Vec::new();
        for (name, install) in installed {
            match catalog.get(name) {
                Some(p) if p.hash() != install.package.hash => {
                    out.push(Sync::Upgrade(p.clone(), install.scope.clone()))
                }
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
            GameAction::RenameWorld { from, to } => {
                if !admin {
                    return Err("only the game's admins rename worlds".into());
                }
                if self.world(from).is_none() {
                    return Err(format!("there's no world \"{from}\""));
                }
                if clean_name(to).as_deref() != Some(to.as_str()) {
                    return Err("a world's name is lowercase letters, digits, - and _".into());
                }
                if self.world(to).is_some() {
                    return Err(format!("there's already a world called \"{to}\""));
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
    let by = Provenance {
        prompt: None,
        ..by.clone()
    };
    serde_json::to_vec(&("game", by, action)).unwrap_or_default()
}

/// What loading game save `name` does in a world whose part of it is change `save`: the
/// change the admin signs for it (whoever keeps the world fills in its state).
pub fn load_action(name: &str, save: u64) -> crate::log::Action {
    crate::log::Action::Load {
        save,
        name: name.into(),
        state: None,
    }
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
        game.push(
            by("ada"),
            GameAction::SetAdmins {
                admins: vec!["ada".into()],
            },
        )
        .unwrap();
        game.push(
            by("ada"),
            GameAction::CreateWorld {
                name: "home".into(),
                seed: 1,
                flat: false,
                recipe: Default::default(),
            },
        )
        .unwrap();
        game.push(
            by("ada"),
            GameAction::CreateWorld {
                name: "sokoban".into(),
                seed: 1,
                flat: true,
                recipe: Default::default(),
            },
        )
        .unwrap();
        assert!(
            game.push(
                by("bo"),
                GameAction::CreateWorld {
                    name: "mine".into(),
                    seed: 1,
                    flat: true,
                    recipe: Default::default()
                }
            )
            .is_err()
        );
        assert!(
            game.push(
                by("ada"),
                GameAction::CreateWorld {
                    name: "home".into(),
                    seed: 2,
                    flat: true,
                    recipe: Default::default()
                }
            )
            .is_err()
        );
        assert!(
            game.push(
                by("ada"),
                GameAction::CreateWorld {
                    name: "Bad Name".into(),
                    seed: 2,
                    flat: true,
                    recipe: Default::default()
                }
            )
            .is_err()
        );
        assert_eq!(game.start().as_deref(), Some("home"));
        assert_eq!(
            game.world("sokoban"),
            Some(Ground {
                seed: 1,
                flat: true,
                recipe: Default::default()
            })
        );
        game.push(
            by("ada"),
            GameAction::RemoveWorld {
                name: "home".into(),
            },
        )
        .unwrap();
        assert!(
            game.push(
                by("ada"),
                GameAction::RemoveWorld {
                    name: "sokoban".into()
                }
            )
            .unwrap_err()
            .contains("at least one")
        );
        assert_eq!(game.start().as_deref(), Some("sokoban"));
        // Anyone saves the game.
        let worlds = BTreeMap::from([("sokoban".to_string(), 7)]);
        game.push(
            by("bo"),
            GameAction::Save {
                name: "before".into(),
                worlds,
            },
        )
        .unwrap();
        assert_eq!(game.saves().count(), 1);
    }

    #[test]
    fn renamed_worlds_keep_their_place_saves_and_portals() {
        let mut game = GameLog::new("local");
        game.push(
            by("ada"),
            GameAction::SetAdmins {
                admins: vec!["ada".into()],
            },
        )
        .unwrap();
        let ground = |seed| GameAction::CreateWorld {
            name: "home".into(),
            seed,
            flat: false,
            recipe: Default::default(),
        };
        game.push(by("ada"), ground(1)).unwrap();
        game.push(
            by("ada"),
            GameAction::CreateWorld {
                name: "arena".into(),
                seed: 2,
                flat: true,
                recipe: Default::default(),
            },
        )
        .unwrap();
        game.push(
            by("ada"),
            GameAction::Save {
                name: "before".into(),
                worlds: BTreeMap::from([("home".into(), 3), ("arena".into(), 4)]),
            },
        )
        .unwrap();
        let rename = |from: &str, to: &str| GameAction::RenameWorld {
            from: from.into(),
            to: to.into(),
        };
        assert!(game.push(by("bo"), rename("home", "base")).is_err());
        assert!(
            game.push(by("ada"), rename("home", "arena"))
                .unwrap_err()
                .contains("already")
        );
        assert!(game.push(by("ada"), rename("home", "Big Base")).is_err());
        assert!(game.push(by("ada"), rename("nowhere", "base")).is_err());
        game.push(by("ada"), rename("home", "base")).unwrap();
        // Same ground, same place in the order: still where players start.
        assert_eq!(game.start().as_deref(), Some("base"));
        assert_eq!(game.world("base").map(|g| g.seed), Some(1));
        assert!(game.world("home").is_none());
        // Portals naming the old name find it; saves name it by its new one.
        assert_eq!(game.current("home").as_deref(), Some("base"));
        assert_eq!(
            game.save_named("before").unwrap(),
            BTreeMap::from([("base".into(), 3), ("arena".into(), 4)])
        );
        // Renamed again, then the old name given to a new world: that one wins.
        game.push(by("ada"), rename("base", "camp")).unwrap();
        assert_eq!(game.current("home").as_deref(), Some("camp"));
        game.push(by("ada"), ground(9)).unwrap();
        assert_eq!(game.current("home").as_deref(), Some("home"));
        assert_eq!(
            game.save_named("before")
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["arena", "camp"]
        );
        // A world taken out leaves its saves.
        game.push(
            by("ada"),
            GameAction::RemoveWorld {
                name: "arena".into(),
            },
        )
        .unwrap();
        assert_eq!(game.current("arena"), None);
        assert_eq!(
            game.save_named("before")
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["camp"]
        );
    }

    #[test]
    fn worlds_follow_the_games_packages() {
        use crate::logic::package::tests::farm;
        let mut game = GameLog::new("g");
        game.push(
            by("ada"),
            GameAction::SetAdmins {
                admins: vec!["ada".into()],
            },
        )
        .unwrap();
        let install = |version| Install {
            package: farm(version).reference(),
            scope: Scope::World,
            by: "ada".into(),
        };
        let installed = |version| BTreeMap::from([("farm".to_string(), install(version))]);
        // A world running a package the game doesn't know yet: the game takes it on.
        assert!(matches!(game.sync(&installed(1)).as_slice(), [Sync::Adopt(n)] if n == "farm"));
        assert!(
            game.push(
                by("bo"),
                GameAction::AddPackage {
                    package: Box::new(farm(1))
                }
            )
            .is_err()
        );
        game.push(
            by("ada"),
            GameAction::AddPackage {
                package: Box::new(farm(1)),
            },
        )
        .unwrap();
        assert!(game.sync(&installed(1)).is_empty());
        // A new version: worlds running the old one move to it.
        game.push(
            by("ada"),
            GameAction::AddPackage {
                package: Box::new(farm(2)),
            },
        )
        .unwrap();
        assert!(
            matches!(game.sync(&installed(1)).as_slice(), [Sync::Upgrade(p, _)] if p.manifest.version == 2)
        );
        // Taken out of the game: worlds stop running it.
        game.push(
            by("ada"),
            GameAction::RemovePackage {
                name: "farm".into(),
            },
        )
        .unwrap();
        assert!(matches!(game.sync(&installed(2)).as_slice(), [Sync::Remove(n)] if n == "farm"));
        assert!(game.packages().is_empty());
    }
}
