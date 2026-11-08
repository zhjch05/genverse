//! A game: what's hosted or joined, holding many worlds (`docs/worlds.md`). Its own
//! small log says which worlds exist, who runs the game, and the saves made of all of
//! them at once. Each world keeps its own log ([`crate::log::WorldLog`]); a game's
//! admins are every world's admins.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::log::Provenance;

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
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GameChange {
    pub id: u64,
    pub by: Provenance,
    pub action: GameAction,
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

    /// Whether `player` may do `action` now.
    pub fn check(&self, player: &str, action: &GameAction) -> Result<(), String> {
        // A new game's first player runs it.
        let admin = self.is_admin(player) || self.admins().is_empty();
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
}
