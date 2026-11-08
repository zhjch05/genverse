//! Entities: the things game rules keep state on (crops, creatures, chests, NPCs).
//! Each one belongs to the package that spawned it and carries named components
//! (`farm:crop`) whose values are plain JSON.
//!
//! Packages never write here directly. They return [`Effect`]s, and a [`Batch`] checks
//! each against what the package may do before any of them lands.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Scope, package::split_name};

/// The most one component's value may weigh, as JSON.
pub const MAX_COMPONENT_BYTES: usize = 4096;

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
}

/// What one installed package may touch.
#[derive(Clone, Debug)]
pub struct Grants {
    pub package: String,
    pub write: Vec<String>,
    pub spawn: bool,
    pub scope: Scope,
    pub max_entities: u32,
}

impl Grants {
    fn writable(&self, component: &str) -> Result<(), String> {
        let Some((ns, _)) = split_name(component) else {
            return Err(format!("component \"{component}\" must be named \"<package>:<name>\""));
        };
        if ns == self.package || self.write.iter().any(|w| w == ns) {
            Ok(())
        } else {
            Err(format!("{} may not change \"{component}\", which belongs to {ns}", self.package))
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
pub struct Entities {
    all: BTreeMap<EntityId, Entity>,
    /// Entities per package, for its limit.
    counts: BTreeMap<String, u32>,
}

impl Entities {
    pub fn get(&self, id: EntityId) -> Option<&Entity> {
        self.all.get(&id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (EntityId, &Entity)> {
        self.all.iter().map(|(id, e)| (*id, e))
    }

    pub fn len(&self) -> usize {
        self.all.len()
    }

    pub fn is_empty(&self) -> bool {
        self.all.is_empty()
    }

    pub fn count(&self, package: &str) -> u32 {
        self.counts.get(package).copied().unwrap_or(0)
    }

    /// Starts a set of effects for change `change`, none applied until [`Batch::finish`].
    pub fn batch(&self, change: u64) -> Batch<'_> {
        Batch { base: self, change, spawned: 0, touched: BTreeMap::new(), counts: BTreeMap::new() }
    }

    /// Lands a finished batch.
    pub fn commit(&mut self, done: Done) {
        for (id, entity) in done.touched {
            match entity {
                Some(e) => self.all.insert(id, e),
                None => self.all.remove(&id),
            };
        }
        for (package, delta) in done.counts {
            let count = self.counts.entry(package).or_default();
            *count = count.saturating_add_signed(delta);
        }
    }
}

/// Effects checked and staged on top of the entities, all or nothing.
pub struct Batch<'a> {
    base: &'a Entities,
    change: u64,
    spawned: u32,
    /// Entities changed so far (`None`: removed).
    touched: BTreeMap<EntityId, Option<Entity>>,
    counts: BTreeMap<String, i32>,
}

/// What a batch changes, to [`Entities::commit`].
pub struct Done {
    touched: BTreeMap<EntityId, Option<Entity>>,
    counts: BTreeMap<String, i32>,
}

impl Batch<'_> {
    pub fn get(&self, id: EntityId) -> Option<&Entity> {
        match self.touched.get(&id) {
            Some(e) => e.as_ref(),
            None => self.base.get(id),
        }
    }

    /// Checks and stages `effect` for the package `grants` describes, or says why not.
    pub fn apply(&mut self, grants: &Grants, effect: &Effect) -> Result<Option<EntityId>, String> {
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
                for (name, value) in components {
                    grants.writable(name)?;
                    check_value(name, value)?;
                }
                let id = EntityId { change: self.change, n: self.spawned };
                self.spawned += 1;
                *self.counts.entry(grants.package.clone()).or_default() += 1;
                let e = Entity { package: grants.package.clone(), position: *position, components: components.clone() };
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
                e.position = *position;
                self.touched.insert(*id, Some(e));
                Ok(None)
            }
            Effect::Set { entity: id, component, value } => {
                let mut e = entity(self, *id)?;
                grants.writable(component)?;
                grants.inside(e.position)?;
                check_value(component, value)?;
                e.components.insert(component.clone(), value.clone());
                self.touched.insert(*id, Some(e));
                Ok(None)
            }
            Effect::Remove { entity: id, component } => {
                let mut e = entity(self, *id)?;
                grants.writable(component)?;
                grants.inside(e.position)?;
                e.components.remove(component);
                self.touched.insert(*id, Some(e));
                Ok(None)
            }
        }
    }

    pub fn finish(self) -> Done {
        Done { touched: self.touched, counts: self.counts }
    }
}

fn own(grants: &Grants, e: &Entity) -> Result<(), String> {
    if e.package == grants.package {
        Ok(())
    } else {
        Err(format!("{} may only move or remove its own entities, not {}'s", grants.package, e.package))
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
        Grants { package: package.into(), write: Vec::new(), spawn: true, scope, max_entities: 2 }
    }

    fn spawn(x: f32) -> Effect {
        Effect::Spawn { position: [x, 0.0, 0.0], components: BTreeMap::from([("farm:crop".into(), json!({ "day": 0 }))]) }
    }

    #[test]
    fn batches_are_all_or_nothing() {
        let mut world = Entities::default();
        let farm = grants("farm", Scope::World);
        let mut batch = world.batch(7);
        let id = batch.apply(&farm, &spawn(1.0)).unwrap().unwrap();
        assert_eq!(id, EntityId { change: 7, n: 0 });
        batch.apply(&farm, &Effect::Set { entity: id, component: "farm:crop".into(), value: json!({ "day": 1 }) }).unwrap();
        // Dropping a batch leaves the world as it was.
        drop(batch);
        assert!(world.is_empty());
        let mut batch = world.batch(7);
        batch.apply(&farm, &spawn(1.0)).unwrap();
        batch.apply(&farm, &spawn(2.0)).unwrap();
        assert!(batch.apply(&farm, &spawn(3.0)).unwrap_err().contains("allows 2"));
        let done = batch.finish();
        world.commit(done);
        assert_eq!((world.len(), world.count("farm")), (2, 2));
    }

    #[test]
    fn packages_keep_to_their_namespace_and_scope() {
        let mut world = Entities::default();
        let farm = grants("farm", Scope::Parcels { parcels: vec![[0, 0]] });
        let mut batch = world.batch(1);
        let id = batch.apply(&farm, &spawn(1.0)).unwrap().unwrap();
        // 8 m is a parcel's edge (32 voxels of 25 cm).
        assert!(batch.apply(&farm, &spawn(9.0)).is_err());
        assert!(batch.apply(&farm, &Effect::Move { entity: id, position: [9.0, 0.0, 0.0] }).is_err());
        world.commit(batch.finish());

        let fish = grants("fish", Scope::World);
        let mut batch = world.batch(2);
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
}
