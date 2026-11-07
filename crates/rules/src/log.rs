//! The world as a log of changes: the seed plus every change, in order, each with who
//! made it. Replaying the log rebuilds the world: placed builds, and the voxel edits on
//! top of the seed (terrain changes compile against the ground the earlier changes
//! left). That's what a save is, what a server would replicate, and what makes
//! rollback a matter of replaying without some changes.
//!
//! Undo never deletes history: it appends a [`Action::Revert`] of the change it undoes.
//!
//! Replay also enforces the land rules ([`crate::land`]) as of each change, so a log
//! can't place things where its author wasn't allowed to, and who owns which parcel
//! falls out of the log too.

use std::collections::{BTreeMap, HashMap, HashSet};

use bevy_math::{IVec2, IVec3, UVec3, Vec3};
use serde::{Deserialize, Serialize};

use crate::{
    build::BuildSpec,
    land::{Land, Spawn, area_parcels, build_parcels},
    sculpt::{self, Area, Columns, TerrainSpec},
    terrain::{Terrain, WORLD_HEIGHT},
    voxel::VoxelId,
};

/// Bumped when the rules change in a way that rasterizes the same log differently.
pub const RULES_VERSION: u32 = 1;

/// Who made a change, and how.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    pub player: String,
    pub player_name: String,
    /// The agent that made it ("Claude Code", "Codex CLI"); none for a person's own
    /// action, like moving or deleting a build.
    pub agent: Option<String>,
    /// The request, kept locally. A server would keep only [`Self::prompt_hash`].
    pub prompt: Option<String>,
    pub prompt_hash: Option<String>,
    /// Unix seconds.
    pub at: u64,
}

impl Provenance {
    pub fn new(player: &str, player_name: &str, agent: Option<&str>, prompt: Option<&str>, at: u64) -> Self {
        Self {
            player: player.into(),
            player_name: player_name.into(),
            agent: agent.map(str::to_string),
            prompt: prompt.map(str::to_string),
            prompt_hash: prompt.map(|p| format!("{:016x}", fnv1a(p.as_bytes()))),
            at,
        }
    }
}

/// A placed build as of some change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BuildState {
    pub build: u64,
    pub name: String,
    /// The request that designed it, and which agent did (shown when it's selected).
    pub prompt: String,
    pub provider: String,
    pub spec: BuildSpec,
    pub position: [f32; 3],
    pub yaw: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    PlaceBuild(BuildState),
    /// Moved, turned, or revised: the build's whole new state.
    UpdateBuild(BuildState),
    RemoveBuild { build: u64 },
    Terrain { name: String, area: Area, spec: TerrainSpec },
    /// Claims free parcels (by parcel coordinates) for the author.
    ClaimParcels { parcels: Vec<[i32; 2]> },
    /// Gives up the author's parcels.
    ReleaseParcels { parcels: Vec<[i32; 2]> },
    /// Undoes an earlier change.
    Revert { change: u64 },
    /// Voxel edits made before the log existed (hand edits), as `[x, y, z, block]`.
    LegacyVoxels { edits: Vec<[i32; 4]> },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub id: u64,
    pub by: Provenance,
    pub action: Action,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorldLog {
    pub seed: u32,
    pub rules_version: u32,
    /// Players who aren't bound by parcels or the spawn protection.
    #[serde(default)]
    pub admins: Vec<String>,
    #[serde(default)]
    pub spawn: Spawn,
    pub changes: Vec<Change>,
}

impl WorldLog {
    pub fn new(seed: u32) -> Self {
        Self { seed, rules_version: RULES_VERSION, admins: Vec::new(), spawn: Spawn::default(), changes: Vec::new() }
    }

    /// Appends a change and returns its id.
    pub fn push(&mut self, by: Provenance, action: Action) -> u64 {
        let id = self.changes.last().map_or(1, |c| c.id + 1);
        self.changes.push(Change { id, by, action });
        id
    }

    /// Changes that count: not reverted, and not reverts themselves.
    pub fn effective(&self) -> impl Iterator<Item = &Change> {
        let reverted: HashSet<u64> = self
            .changes
            .iter()
            .filter_map(|c| match c.action {
                Action::Revert { change } => Some(change),
                _ => None,
            })
            .collect();
        self.changes
            .iter()
            .filter(move |c| !reverted.contains(&c.id) && !matches!(c.action, Action::Revert { .. }))
    }

    /// A stable fingerprint of the whole log, for checking a cache is current.
    pub fn hash(&self) -> u64 {
        fnv1a(serde_json::to_string(self).unwrap_or_default().as_bytes())
    }

    /// The same log without every change `drop` matches (and reverts of those):
    /// rollback, e.g. of one player's changes or a time range.
    pub fn without(&self, mut drop: impl FnMut(&Change) -> bool) -> Self {
        let dropped: HashSet<u64> = self.changes.iter().filter(|c| drop(c)).map(|c| c.id).collect();
        let changes = self
            .changes
            .iter()
            .filter(|c| match c.action {
                Action::Revert { change } => !dropped.contains(&c.id) && !dropped.contains(&change),
                _ => !dropped.contains(&c.id),
            })
            .cloned()
            .collect();
        Self { changes, ..self.clone() }
    }
}

/// 64-bit FNV-1a: tiny, and stable across Rust versions (unlike `DefaultHasher`).
pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ *b as u64).wrapping_mul(0x0000_0100_0000_01b3))
}

/// The world a log describes.
#[derive(Default)]
pub struct Replay {
    pub builds: BTreeMap<u64, BuildState>,
    /// Voxel edits on top of the seed.
    pub edits: HashMap<IVec3, VoxelId>,
    pub land: Land,
    /// Changes that no longer apply (refused by the land rules, or by tightened
    /// limits), with why.
    pub problems: Vec<String>,
}

pub fn parcels_of(build: &BuildState) -> Vec<IVec2> {
    build_parcels(Vec3::from_array(build.position), build.yaw, &build.spec.size)
}

/// Rebuilds everything: builds, land, and voxel edits.
pub fn replay(log: &WorldLog) -> Replay {
    walk(log, true)
}

/// Builds and land only, skipping the voxel work: cheap enough to redo after every change.
pub fn replay_land(log: &WorldLog) -> Replay {
    walk(log, false)
}

fn walk(log: &WorldLog, voxels: bool) -> Replay {
    let terrain = Terrain { seed: log.seed };
    let mut out = Replay { land: Land::new(log.admins.clone(), log.spawn.clone()), ..Default::default() };
    for change in log.effective() {
        let by = &change.by;
        let refused = |why: String| format!("change {} by {}: {why}", change.id, by.player_name);
        let parcels = |list: &[[i32; 2]]| list.iter().map(|p| IVec2::from_array(*p)).collect::<Vec<_>>();
        match &change.action {
            Action::PlaceBuild(b) | Action::UpdateBuild(b) => {
                // Moving a build needs rights where it was and where it goes.
                let mut touched = parcels_of(b);
                touched.extend(out.builds.get(&b.build).map(parcels_of).unwrap_or_default());
                match out.land.authorize(&by.player, &touched) {
                    Ok(claims) => {
                        out.land.claim(&by.player, &by.player_name, &claims);
                        out.builds.insert(b.build, b.clone());
                    }
                    Err(why) => out.problems.push(refused(why)),
                }
            }
            Action::RemoveBuild { build } => {
                let touched = out.builds.get(build).map(parcels_of).unwrap_or_default();
                match out.land.authorize(&by.player, &touched) {
                    Ok(_) => {
                        out.builds.remove(build);
                    }
                    Err(why) => out.problems.push(refused(why)),
                }
            }
            Action::Terrain { name, area, spec } => {
                let claims = match out.land.authorize(&by.player, &area_parcels(*area)) {
                    Ok(claims) => claims,
                    Err(why) => {
                        out.problems.push(refused(why));
                        continue;
                    }
                };
                if voxels {
                    let columns = ground_columns(terrain, *area, &out.edits);
                    match sculpt::compile(spec, *area, terrain, columns) {
                        Ok(edit) => {
                            let (_, edits) = out
                                .land
                                .authorize_terrain(&by.player, *area, edit.edits)
                                .expect("authorized above");
                            out.edits.extend(edits);
                        }
                        Err(errors) => out.problems.push(refused(format!("\"{name}\" {}", errors.join("; ")))),
                    }
                }
                out.land.claim(&by.player, &by.player_name, &claims);
            }
            Action::ClaimParcels { parcels: list } => match out.land.authorize(&by.player, &parcels(list)) {
                Ok(claims) => out.land.claim(&by.player, &by.player_name, &claims),
                Err(why) => out.problems.push(refused(why)),
            },
            Action::ReleaseParcels { parcels: list } => out.land.release(&by.player, &parcels(list)),
            Action::LegacyVoxels { edits } => {
                if voxels {
                    out.edits.extend(edits.iter().map(|[x, y, z, id]| (IVec3::new(*x, *y, *z), *id as VoxelId)));
                }
            }
            Action::Revert { .. } => {}
        }
    }
    out
}

/// The columns a terrain change in `area` reads, generated from the seed with `edits`
/// on top: what the loaded world would show, without needing it loaded.
pub fn ground_columns(terrain: Terrain, area: Area, edits: &HashMap<IVec3, VoxelId>) -> Columns {
    let (min, size) = Columns::bounds(area);
    let origin = IVec3::new(min.x, 0, min.y);
    let mut grid = terrain.generate(origin, UVec3::new(size.x as u32, WORLD_HEIGHT as u32, size.y as u32));
    let max = min + size;
    for (p, id) in edits {
        let c = IVec2::new(p.x, p.z);
        if c.cmpge(min).all() && c.cmplt(max).all() {
            grid.set(*p - origin, *id);
        }
    }
    Columns::read(area, |x, z, out| {
        for (y, v) in out.iter_mut().enumerate() {
            *v = grid.get(IVec3::new(x - min.x, y as i32, z - min.y));
        }
        true
    })
    .expect("generated columns are always available")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sculpt::{OpArea, TerrainOp};

    fn by(player: &str) -> Provenance {
        Provenance::new(player, player, Some("Claude Code"), Some("a pond"), 0)
    }

    fn pond(x: i32) -> Action {
        let op = TerrainOp {
            kind: "water".into(),
            area: Some(OpArea { shape: "ellipse".into(), x: 1.0, z: 1.0, w: 4.0, d: 4.0 }),
            points: None,
            amount_m: None,
            level_m: None,
            depth_m: Some(1.0),
            width_m: None,
            edge_m: Some(1.0),
            material: None,
            item: None,
            density: None,
        };
        Action::Terrain {
            name: "Pond".into(),
            area: Area { min: [x, 0], size: [24, 24] },
            spec: TerrainSpec { name: "Pond".into(), ops: vec![op] },
        }
    }

    fn lamp(build: u64) -> Action {
        let spec: BuildSpec = serde_json::from_str(
            r##"{"name":"Lamp","size":[3,4,3],"palette":[{"name":"iron","hex":"#3d4248","material":"metal"}],
                "shapes":[{"kind":"box","color":"iron","from":[0,0,0],"to":[3,4,3],"radius":null,"wobble":null}]}"##,
        )
        .unwrap();
        Action::PlaceBuild(BuildState {
            build,
            name: "Lamp".into(),
            prompt: "a lamp".into(),
            provider: "Claude Code".into(),
            spec,
            position: [0.0, 10.0, 0.0],
            yaw: 0.0,
        })
    }

    #[test]
    fn replay_is_deterministic_and_survives_json() {
        let mut log = WorldLog::new(1337);
        // Admins aren't bound by land, which has its own test.
        log.admins = vec!["a".into(), "b".into()];
        log.push(by("a"), pond(400));
        log.push(by("a"), lamp(1));
        log.push(by("b"), pond(410));
        let first = replay(&log);
        assert!(!first.edits.is_empty() && first.problems.is_empty(), "{:?}", first.problems);
        let reloaded: WorldLog = serde_json::from_str(&serde_json::to_string(&log).unwrap()).unwrap();
        assert_eq!(reloaded.hash(), log.hash());
        let second = replay(&reloaded);
        assert_eq!(first.edits, second.edits);
        assert_eq!(first.builds, second.builds);
    }

    #[test]
    fn revert_and_rollback() {
        let mut log = WorldLog::new(1337);
        log.admins = vec!["a".into(), "b".into()];
        let placed = log.push(by("a"), lamp(1));
        log.push(by("b"), lamp(2));
        log.push(by("a"), Action::Revert { change: placed });
        assert_eq!(replay(&log).builds.keys().copied().collect::<Vec<_>>(), vec![2]);

        // Rolling back player b leaves a's history (and its revert) intact.
        let without_b = log.without(|c| c.by.player == "b");
        assert!(replay(&without_b).builds.is_empty());
        // Rolling back a's revert brings its lamp back.
        let without_revert = log.without(|c| matches!(c.action, Action::Revert { .. }));
        assert_eq!(replay(&without_revert).builds.len(), 2);
    }

    #[test]
    fn replay_enforces_land() {
        let mut log = WorldLog::new(1337);
        log.push(by("a"), lamp(1));
        // Lamp 1 stands at the origin, in the spawn: only admins may, so it's refused...
        assert_eq!(replay_land(&log).problems.len(), 1);
        // ...until a is an admin.
        log.admins.push("a".into());
        let land = replay_land(&log);
        assert!(land.problems.is_empty() && land.builds.len() == 1);

        // b claims land by building on it; c can't build there, a (admin) can.
        let mut away = lamp(2);
        if let Action::PlaceBuild(b) = &mut away {
            b.position = [100.0, 10.0, 100.0];
        }
        log.push(by("b"), away.clone());
        let mut c = away.clone();
        if let Action::PlaceBuild(b) = &mut c {
            b.build = 3;
        }
        log.push(by("c"), c);
        let land = replay_land(&log);
        assert_eq!(land.builds.keys().copied().collect::<Vec<_>>(), vec![1, 2]);
        assert!(land.problems[0].contains("belongs to b"), "{:?}", land.problems);
        assert!(land.land.parcels.values().all(|p| p.owner == "b"));

        // Rolling b back frees the land, and c's lamp stands after all.
        let rolled = replay_land(&log.without(|ch| ch.by.player == "b"));
        assert_eq!(rolled.builds.keys().copied().collect::<Vec<_>>(), vec![1, 3]);
    }

    #[test]
    fn generated_columns_match_however_the_region_is_cut() {
        // Chunks generate in pieces; replay generates whole areas. They must agree.
        let terrain = Terrain { seed: 1337 };
        let a = terrain.generate(IVec3::new(390, 0, -12), UVec3::new(48, WORLD_HEIGHT as u32, 48));
        let b = terrain.generate(IVec3::new(400, 0, 0), UVec3::new(32, WORLD_HEIGHT as u32, 32));
        for z in 0..32 {
            for y in 0..WORLD_HEIGHT {
                for x in 0..32 {
                    let p = IVec3::new(400 + x, y, z);
                    assert_eq!(a.get(p - IVec3::new(390, 0, -12)), b.get(p - IVec3::new(400, 0, 0)), "at {p}");
                }
            }
        }
    }
}
