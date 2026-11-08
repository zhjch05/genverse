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
//! - `zone`: `{"size": [w, d]}`, a rectangle centered on it. A player who walks in hands
//!   their movement keys to its package (as [`super::Intent::Key`]) instead of walking.
//! - `pawn`: a player id. That player's hero stands where this entity is, so a game
//!   moves its players by moving their pawns.
//!
//! Packages never write here directly. Each change goes through a [`Batch`], which checks
//! it against what the package may do; the batch lands whole, or not at all.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Scope, package::split_name};

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

#[derive(Clone, Debug, PartialEq)]
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
}

/// Components the engine itself reads.
pub const RESERVED: [&str; 4] = ["look", "label", "zone", "pawn"];

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
            model && turn
        }
        "label" => value.as_str().is_some_and(|l| l.chars().count() <= 60),
        "zone" => sizes("size", 2, 128.0),
        "pawn" => value.as_str().is_some_and(|p| !p.is_empty() && p.len() <= 128),
        _ => true,
    };
    if ok {
        Ok(())
    } else {
        Err(match name {
            "look" => format!(
                "\"look\" is {{\"model\": one of the package's models ({}), \"turn\": 0-3}}",
                if grants.models.is_empty() { "it has none".to_string() } else { grants.models.join(", ") }
            ),
            "label" => "\"label\" is text of up to 60 characters".into(),
            "zone" => "\"zone\" is {\"size\": [w, d]} (meters, 0-128)".into(),
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
    /// Every entity it makes must be drawn: a `look`, unless it's a `pawn` (the hero
    /// shows those). True for scripted packages; the simple farming kinds draw
    /// themselves.
    pub drawn: bool,
}

impl Grants {
    /// Whether it may change the component or item `name`.
    fn writable(&self, name: &str) -> Result<(), String> {
        let Some((ns, _)) = split_name(name) else {
            return Err(format!("\"{name}\" must be named \"<package>:<name>\""));
        };
        if ns == self.package || self.write.iter().any(|w| w == ns) {
            Ok(())
        } else {
            Err(format!("{} may not change \"{name}\", which belongs to {ns}", self.package))
        }
    }

    fn inside(&self, position: [f32; 3]) -> Result<(), String> {
        if self.scope.contains(position) {
            Ok(())
        } else {
            Err(format!("{} works only where it's installed", self.package))
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct GameState {
    entities: BTreeMap<EntityId, Entity>,
    /// Entities per package, for its limit.
    counts: BTreeMap<String, u32>,
    /// What each player carries: item name to how many (never 0).
    items: BTreeMap<String, BTreeMap<String, u32>>,
}

impl GameState {
    pub fn get(&self, id: EntityId) -> Option<&Entity> {
        self.entities.get(&id)
    }

    pub fn entities(&self) -> impl Iterator<Item = (EntityId, &Entity)> {
        self.entities.iter().map(|(id, e)| (*id, e))
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
        }
    }

    /// Lands a finished batch.
    pub fn commit(&mut self, done: Done) {
        for (id, entity) in done.touched {
            match entity {
                Some(e) => self.entities.insert(id, e),
                None => self.entities.remove(&id),
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
}

/// What a batch changes, to [`GameState::commit`].
pub struct Done {
    touched: BTreeMap<EntityId, Option<Entity>>,
    counts: BTreeMap<String, i32>,
    items: BTreeMap<(String, String), u32>,
    /// Messages for players: who, and what.
    pub notes: Vec<(String, String)>,
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
    pub fn ids(&self) -> Vec<EntityId> {
        let base = self.base.entities.keys().filter(|id| !self.touched.contains_key(id));
        let staged = self.touched.iter().filter(|(_, e)| e.is_some()).map(|(id, _)| id);
        let mut ids: Vec<EntityId> = base.chain(staged).copied().collect();
        ids.sort_unstable();
        ids
    }

    /// The player whose action this is.
    pub fn actor(&self) -> &str {
        self.actor
    }

    /// Checks and stages `effect` for the package `grants` describes, or says why not.
    pub fn apply(&mut self, grants: &Grants, effect: &Effect) -> Result<Option<EntityId>, String> {
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
                if grants.drawn && !components.contains_key("look") && !components.contains_key("pawn") {
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
                    if component == "look" && grants.drawn && !e.components.contains_key("pawn") {
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
        }
    }

    pub fn finish(self) -> Done {
        Done { touched: self.touched, counts: self.counts, items: self.items, notes: self.notes }
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
        Grants { package: package.into(), write: Vec::new(), spawn: true, scope, max_entities: 2, models: vec!["crate".into()], drawn: false }
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
