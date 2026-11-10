//! Undo as a change of its own. When a change that can be undone applies, replay keeps
//! what it replaced ([`UndoRecord`]); undoing it ([`crate::log::Action::Undo`]) puts that
//! back on the world as it is now, and is refused if anything has changed there since.
//! So the log only moves forward: nothing is ever taken out of it, and undoing never
//! replays anything. Undoing an undo redoes the change.
//!
//! Builds (placing, changing, removing, portals), land changes, the rules and installing
//! packages can be undone; what games do can't (a game move is a move), and neither
//! can loading a save (load the save made before it instead). Only the latest
//! [`UNDO_DEPTH`] changes keep their records.

use bevy_math::IVec3;
use serde::{Deserialize, Serialize};

use crate::{
    budget::{Footprint, build_usage},
    land::area_parcels,
    log::{BuildState, PortalTo, Provenance, Replay, parcels_of, terrain_footprint},
    logic::Install,
    sculpt::Area,
    settings::WorldRules,
    voxel::VoxelId,
};

/// How many of the latest changes can still be undone.
pub const UNDO_DEPTH: usize = 100;

/// A build as it stood: its state, whose budget it's on, and where it leads if it's a
/// portal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Placed {
    pub build: BuildState,
    pub owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portal: Option<PortalTo>,
}

/// One voxel a change set: where, and its edit and what it showed, before and after. An
/// edit of `None` is the seed's ground.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    pub p: [i32; 3],
    pub from: (Option<VoxelId>, VoxelId),
    pub to: (Option<VoxelId>, VoxelId),
}

impl Cell {
    pub fn pos(&self) -> IVec3 {
        IVec3::from_array(self.p)
    }
}

/// One part of what a change did, as from → to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Op {
    /// A build appearing, changing or going, with its portal.
    Build { id: u64, from: Option<Box<Placed>>, to: Option<Box<Placed>> },
    /// Land reshaped: its voxels, and what it costs `owner` (`charge` adds the cost;
    /// otherwise it's given back).
    Land { owner: String, area: Area, cost: usize, charge: bool, cells: Vec<Cell> },
    Rules { from: Box<WorldRules>, to: Box<WorldRules> },
    Install { name: String, from: Option<Install>, to: Option<Install> },
}

impl Op {
    /// The op that takes it back.
    fn inverse(&self) -> Op {
        match self {
            Op::Build { id, from, to } => Op::Build { id: *id, from: to.clone(), to: from.clone() },
            Op::Land { owner, area, cost, charge, cells } => Op::Land {
                owner: owner.clone(),
                area: *area,
                cost: *cost,
                charge: !charge,
                cells: cells.iter().rev().map(|c| Cell { p: c.p, from: c.to, to: c.from }).collect(),
            },
            Op::Rules { from, to } => Op::Rules { from: to.clone(), to: from.clone() },
            Op::Install { name, from, to } => Op::Install { name: name.clone(), from: to.clone(), to: from.clone() },
        }
    }
}

/// What one change did, to undo it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UndoRecord {
    /// Who made the change: they (and admins) may undo it.
    pub by: String,
    /// Whether the change was itself an undo: undoing it redoes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redo: bool,
    pub ops: Vec<Op>,
}

impl Replay {
    /// Build `id` as it stands now.
    pub fn placed(&self, id: u64) -> Option<Placed> {
        let build = self.builds.get(&id)?.clone();
        let owner = self.footprints.get(&id).map_or(String::new(), |f| f.owner.clone());
        Some(Placed { build, owner, portal: self.portals.get(&id).cloned() })
    }

    /// Keeps what change `change` did, so it can be undone; the oldest records go.
    pub(crate) fn remember(&mut self, change: u64, by: &str, redo: bool, ops: Vec<Op>) {
        if ops.is_empty() {
            return;
        }
        self.undoable.insert(change, UndoRecord { by: by.into(), redo, ops });
        while self.undoable.len() > UNDO_DEPTH {
            self.undoable.pop_first();
        }
    }

    /// Undoes change `target` as change `change` by `player`, on the world as it is now:
    /// all of it or, if anything it touched has changed since, none of it. Returns the
    /// voxels it set back.
    pub(crate) fn undo(&mut self, change: u64, target: u64, by: &Provenance, admin: bool, voxels: bool) -> Result<Vec<(IVec3, VoxelId)>, String> {
        let player = by.player.as_str();
        let record = self
            .undoable
            .get(&target)
            .ok_or(format!("change {target} can't be undone (it's undone already, too old, or a kind that can't be)"))?;
        if record.by != player && !admin {
            return Err("you can only undo your own changes".into());
        }
        let ops: Vec<Op> = record.ops.iter().rev().map(Op::inverse).collect();
        // Everything is checked first; the world changes only after.
        let mut budgets = self.budgets.clone();
        let mut logic = None;
        for op in &ops {
            match op {
                Op::Build { id, from, to } => {
                    if self.placed(*id).map(Box::new) != *from {
                        return Err(format!("build {id} has changed since"));
                    }
                    let mut touched = from.as_ref().map(|p| parcels_of(&p.build)).unwrap_or_default();
                    touched.extend(to.as_ref().map(|p| parcels_of(&p.build)).unwrap_or_default());
                    self.land.authorize(player, &touched)?;
                    let old = from.as_ref().map(|p| footprint(p));
                    if let Some(new) = to.as_ref().map(|p| footprint(p)) {
                        budgets.check(&new, old.as_ref(), &self.rules.budgets)?;
                        budgets.add(&new);
                    }
                    if let Some(old) = old {
                        budgets.remove(&old);
                    }
                }
                Op::Land { owner, area, cost, charge, cells } => {
                    if voxels && cells.iter().any(|c| self.edits.get(&c.pos()).copied() != c.from.0) {
                        return Err("the ground there has changed since".into());
                    }
                    self.land.authorize(player, &area_parcels(*area))?;
                    let cost = terrain_footprint(owner, *cost);
                    if *charge {
                        budgets.check(&cost, None, &self.rules.budgets)?;
                        budgets.add(&cost);
                    } else {
                        budgets.remove(&cost);
                    }
                }
                Op::Rules { from, .. } => {
                    if !admin {
                        return Err("only admins change the rules".into());
                    }
                    if self.rules != **from {
                        return Err("the rules have changed since".into());
                    }
                }
                Op::Install { name, from, to } => {
                    let logic = logic.get_or_insert_with(|| self.logic.clone());
                    if logic.installed.get(name) != from.as_ref() {
                        return Err(format!("{name} has changed since"));
                    }
                    match to {
                        Some(install) => logic.install(player, admin, &self.rules.logic, &install.package, &install.scope)?,
                        None => logic.uninstall(player, admin, &self.rules.logic, name)?,
                    }
                }
            }
        }
        // It all fits: do it, keeping what it did (to redo).
        self.budgets = budgets;
        if let Some(logic) = logic {
            self.logic = logic;
        }
        let mut made = Vec::new();
        let mut done = Vec::new();
        for op in ops {
            match op {
                Op::Build { id, from, to } => {
                    self.builds.remove(&id);
                    self.footprints.remove(&id);
                    self.portals.remove(&id);
                    match &to {
                        Some(placed) => {
                            self.footprints.insert(id, footprint(placed));
                            self.builds.insert(id, placed.build.clone());
                            if let Some(portal) = &placed.portal {
                                self.portals.insert(id, portal.clone());
                            }
                        }
                        None => self.logic.state.forget_build(id),
                    }
                    done.push(Op::Build { id, from, to });
                }
                Op::Land { owner, area, cost, charge, cells } => {
                    for cell in &cells {
                        if voxels {
                            match cell.to.0 {
                                Some(v) => self.edits.insert(cell.pos(), v),
                                None => self.edits.remove(&cell.pos()),
                            };
                        }
                        made.push((cell.pos(), cell.to.1));
                    }
                    self.last_cells.extend(cells.iter().copied());
                    done.push(Op::Land { owner, area, cost, charge, cells });
                }
                Op::Rules { from, to } => {
                    self.land.spawn = to.spawn.clone();
                    self.rules = (*to).clone();
                    done.push(Op::Rules { from, to });
                }
                Op::Install { name, from, .. } => {
                    let to = self.logic.installed.get(&name).cloned();
                    done.push(Op::Install { name, from, to });
                }
            }
        }
        self.undoable.remove(&target);
        self.remember(change, player, true, done);
        Ok(made)
    }
}

fn footprint(placed: &Placed) -> Footprint {
    Footprint { owner: placed.owner.clone(), parcels: parcels_of(&placed.build), usage: build_usage(&placed.build.spec) }
}
