//! Agent-made objects: a draft preview, placed creations, selection, editing, undo,
//! and saving to disk.

use std::f32::consts::{FRAC_PI_2, PI};

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use super::spec::{BuildSpec, BuildStats, VOXEL_SIZE, compile};
use crate::{
    voxel::{DepthBias, VoxelModel, VoxelModelAsset},
    world::{SolidModel, VoxelWorld, terrain::is_solid},
};

const SAVE_PATH: &str = "saves/creations.json";

/// What gets written to disk: the spec (not voxels), so saves stay small and are
/// rebuilt with the current rules on load.
#[derive(Clone, Serialize, Deserialize)]
pub struct SavedCreation {
    pub id: u64,
    pub name: String,
    pub prompt: String,
    pub provider: String,
    pub position: [f32; 3],
    pub yaw: f32,
    pub spec: BuildSpec,
}

pub struct Draft {
    pub entity: Entity,
    pub spec: BuildSpec,
    pub stats: BuildStats,
    pub prompt: String,
    pub provider: String,
    /// The placed build this draft is an edit of; restored if the draft is discarded.
    pub replaces: Option<SavedCreation>,
    /// Height above the surface under the cursor (R/F), in meters.
    pub lift: f32,
    /// Set while the draft overlaps the hero; it can't be placed there.
    pub blocked: bool,
    /// Following drafts stick to the cursor; pinned ones (revisions of a placed build)
    /// stay put until picked up.
    pub follow: bool,
}

#[derive(Resource, Default)]
pub struct Creations {
    pub placed: Vec<(Entity, SavedCreation)>,
    pub draft: Option<Draft>,
    pub selected: Option<Entity>,
    /// Changes this session, newest last, for Undo.
    history: Vec<Change>,
    next_id: u64,
}

/// One undoable change to the placed builds.
enum Change {
    Placed(u64),
    Deleted(SavedCreation),
    /// A build was moved or revised; holds how it was before.
    Replaced(SavedCreation),
    /// A kept terrain change, undone by the landscape (by id).
    Terrain(u64),
}

/// What [`Creations::undo`] did, or left for the caller to do.
pub enum Undone {
    Message(String),
    Terrain(u64),
}

/// Marks a creation entity; drafts also carry [`DraftMarker`].
#[derive(Component)]
pub struct Creation {
    pub size: Vec3,
}

#[derive(Component)]
pub struct DraftMarker;

/// Spawns a placed (solid) creation.
fn spawn_placed(commands: &mut Commands, model: &VoxelModelAsset, saved: &SavedCreation) -> Entity {
    let entity = spawn(commands, model, &saved.name, saved.position.into(), saved.yaw);
    commands.entity(entity).insert(SolidModel);
    entity
}

fn spawn(commands: &mut Commands, model: &VoxelModelAsset, name: &str, position: Vec3, yaw: f32) -> Entity {
    let size = model.grid.size().as_vec3() * VOXEL_SIZE;
    commands
        .spawn((
            Name::new(format!("Creation: {name}")),
            Creation { size },
            VoxelModel::from(model),
            // Builds sit on the terrain/water grid, so their faces often coincide with it.
            DepthBias(500.0),
            Transform::from_translation(position).with_rotation(Quat::from_rotation_y(yaw)),
        ))
        .id()
}

/// A spot in front of the player, on the ground, with the build's front facing them.
pub fn anchor_in_front(player: &Transform, world: &VoxelWorld, size: UVec3) -> (Vec3, f32) {
    let facing = player.rotation.to_euler(EulerRot::YXZ).0;
    let forward = player.rotation * Vec3::NEG_Z;
    let depth = size.z as f32 * VOXEL_SIZE;
    let mut position = player.translation + forward * (depth / 2.0 + 1.5);
    position.x = (position.x / VOXEL_SIZE).round() * VOXEL_SIZE;
    position.z = (position.z / VOXEL_SIZE).round() * VOXEL_SIZE;

    // Sit on the highest solid voxel within a few meters of the player's height.
    let start = VoxelWorld::voxel_at(position + Vec3::Y * 4.0);
    for dy in 0..48 {
        let p = start - IVec3::Y * dy;
        if world.get(p).is_some_and(is_solid) {
            position.y = (p.y + 1) as f32 * crate::world::terrain::VOXEL_SIZE;
            break;
        }
    }
    let yaw = ((facing + PI) / FRAC_PI_2).round() * FRAC_PI_2;
    (position, yaw)
}

impl Creations {
    /// Shows a new draft at `at`. A revision of an existing draft keeps its link to
    /// the build it replaces.
    pub fn set_draft(
        &mut self,
        commands: &mut Commands,
        model: &VoxelModelAsset,
        draft: (BuildSpec, BuildStats, String, String),
        at: (Vec3, f32),
    ) {
        let (replaces, lift, follow) = self.draft.take().map_or((None, 0.0, true), |old| {
            commands.entity(old.entity).despawn();
            (old.replaces, old.lift, old.follow)
        });
        let (spec, stats, prompt, provider) = draft;
        let entity = spawn(commands, model, &spec.name, at.0, at.1);
        commands.entity(entity).insert(DraftMarker);
        self.draft = Some(Draft { entity, spec, stats, prompt, provider, replaces, lift, blocked: false, follow });
    }

    /// Drops the draft; if it was an edit, the original build comes back unchanged.
    pub fn discard_draft(&mut self, commands: &mut Commands) {
        let Some(draft) = self.draft.take() else {
            return;
        };
        commands.entity(draft.entity).despawn();
        if let Some(original) = draft.replaces
            && let Ok(build) = compile(&original.spec)
        {
            let entity = spawn_placed(commands, &build.model, &original);
            self.placed.push((entity, original));
        }
    }

    /// Turns the selected placed build back into a draft: `follow` picks it up to move
    /// it; otherwise it stays pinned in place to be revised.
    pub fn edit_selected(&mut self, commands: &mut Commands, follow: bool) -> Option<String> {
        if self.draft.is_some() {
            return None;
        }
        let entity = self.selected?;
        let index = self.placed.iter().position(|(e, _)| *e == entity)?;
        let stats = compile(&self.placed[index].1.spec).ok()?.stats;
        let (_, saved) = self.placed.remove(index);
        self.selected = None;
        commands.entity(entity).remove::<SolidModel>().insert(DraftMarker);
        let name = saved.name.clone();
        self.draft = Some(Draft {
            entity,
            spec: saved.spec.clone(),
            stats,
            prompt: saved.prompt.clone(),
            provider: saved.provider.clone(),
            replaces: Some(saved),
            lift: 0.0,
            blocked: false,
            follow,
        });
        Some(name)
    }

    pub fn delete_selected(&mut self, commands: &mut Commands) -> Option<String> {
        let entity = self.selected.take()?;
        let index = self.placed.iter().position(|(e, _)| *e == entity)?;
        let (_, saved) = self.placed.remove(index);
        commands.entity(entity).despawn();
        let name = saved.name.clone();
        self.history.push(Change::Deleted(saved));
        self.save();
        Some(name)
    }

    /// Starts a new following draft that copies a placed build (Minecraft's pick block).
    pub fn copy_as_draft(&mut self, commands: &mut Commands, entity: Entity, at: &Transform) -> Option<String> {
        if self.draft.is_some() {
            return None;
        }
        let saved = self.placed.iter().find(|(e, _)| *e == entity).map(|(_, c)| c.clone())?;
        let build = compile(&saved.spec).ok()?;
        let yaw = at.rotation.to_euler(EulerRot::YXZ).0;
        let draft = spawn(commands, &build.model, &saved.name, at.translation, yaw);
        commands.entity(draft).insert(DraftMarker);
        self.selected = None;
        self.draft = Some(Draft {
            entity: draft,
            spec: saved.spec,
            stats: build.stats,
            prompt: saved.prompt,
            provider: saved.provider,
            replaces: None,
            lift: 0.0,
            blocked: false,
            follow: true,
        });
        Some(saved.name)
    }

    /// Records a kept terrain change, so Undo reverts changes in the order they were made.
    pub fn record_terrain(&mut self, id: u64) {
        self.history.push(Change::Terrain(id));
    }

    pub fn can_undo(&self) -> bool {
        self.draft.is_none() && !self.history.is_empty()
    }

    pub fn selected(&self) -> Option<&SavedCreation> {
        let entity = self.selected?;
        self.placed.iter().find(|(e, _)| *e == entity).map(|(_, c)| c)
    }

    /// Makes the draft permanent where it stands (`transform` is the draft's).
    pub fn place_draft(&mut self, commands: &mut Commands, transform: &Transform) -> Option<String> {
        if self.draft.as_ref()?.blocked {
            return None;
        }
        let draft = self.draft.take()?;
        commands.entity(draft.entity).remove::<DraftMarker>().insert(SolidModel);
        self.next_id = self.next_id.max(self.placed.iter().map(|(_, c)| c.id + 1).max().unwrap_or(0));
        // An edit keeps its original id.
        let id = draft.replaces.as_ref().map(|r| r.id).unwrap_or_else(|| {
            self.next_id += 1;
            self.next_id - 1
        });
        let saved = SavedCreation {
            id,
            name: draft.spec.name.clone(),
            prompt: draft.prompt,
            provider: draft.provider,
            position: transform.translation.into(),
            yaw: transform.rotation.to_euler(EulerRot::YXZ).0,
            spec: draft.spec,
        };
        let name = saved.name.clone();
        self.history.push(match draft.replaces {
            Some(before) => Change::Replaced(before),
            None => Change::Placed(id),
        });
        self.placed.push((draft.entity, saved));
        // Stay selected so a follow-up chat message ("make it blue") applies to it.
        self.selected = Some(draft.entity);
        self.save();
        Some(name)
    }

    /// Reverts the latest change; returns a message describing what happened.
    pub fn undo(&mut self, commands: &mut Commands) -> Option<Undone> {
        if self.draft.is_some() {
            return None;
        }
        let change = self.history.pop()?;
        let message = match change {
            Change::Placed(id) => {
                let (_, saved) = self.take_placed(commands, id)?;
                format!("Removed \"{}\".", saved.name)
            }
            Change::Deleted(saved) => {
                let name = saved.name.clone();
                self.restore(commands, saved)?;
                format!("Brought back \"{name}\".")
            }
            Change::Replaced(before) => {
                self.take_placed(commands, before.id)?;
                let name = before.name.clone();
                self.restore(commands, before)?;
                format!("Put \"{name}\" back how it was.")
            }
            Change::Terrain(id) => return Some(Undone::Terrain(id)),
        };
        self.save();
        Some(Undone::Message(message))
    }

    fn take_placed(&mut self, commands: &mut Commands, id: u64) -> Option<(Entity, SavedCreation)> {
        let index = self.placed.iter().position(|(_, c)| c.id == id)?;
        let (entity, saved) = self.placed.remove(index);
        if self.selected == Some(entity) {
            self.selected = None;
        }
        commands.entity(entity).despawn();
        Some((entity, saved))
    }

    fn restore(&mut self, commands: &mut Commands, saved: SavedCreation) -> Option<Entity> {
        let build = compile(&saved.spec).ok()?;
        let entity = spawn_placed(commands, &build.model, &saved);
        self.placed.push((entity, saved));
        Some(entity)
    }

    fn save(&self) {
        // A build being edited stays saved as it was until the edit is placed.
        let editing = self.draft.as_ref().and_then(|d| d.replaces.as_ref());
        let list: Vec<_> = self.placed.iter().map(|(_, c)| c).chain(editing).collect();
        let result = std::fs::create_dir_all("saves").and_then(|_| {
            std::fs::write(SAVE_PATH, serde_json::to_string_pretty(&list).unwrap_or_default())
        });
        if let Err(e) = result {
            error!("couldn't save creations to {SAVE_PATH}: {e}");
        }
    }
}

pub fn load_creations(mut commands: Commands, mut creations: ResMut<Creations>) {
    let Ok(text) = std::fs::read_to_string(SAVE_PATH) else {
        return;
    };
    let saved: Vec<SavedCreation> = match serde_json::from_str(&text) {
        Ok(saved) => saved,
        Err(e) => {
            error!("couldn't read {SAVE_PATH}: {e}");
            return;
        }
    };
    for c in saved {
        // Saved specs are re-validated, so tightened limits apply to old builds too.
        match compile(&c.spec) {
            Ok(build) => {
                let entity = spawn_placed(&mut commands, &build.model, &c);
                creations.placed.push((entity, c));
            }
            Err(errors) => warn!("skipping saved creation {:?}: {}", c.name, errors.join("; ")),
        }
    }
}

/// Outlines the draft (pulsing yellow, "not placed yet") and the selection (cyan).
pub fn outline_creations(
    time: Res<Time>,
    creations: Res<Creations>,
    mut gizmos: Gizmos,
    query: Query<(Entity, &Transform, &Creation, Has<DraftMarker>)>,
) {
    let pulse = 0.6 + 0.4 * (time.elapsed_secs() * 3.0).sin();
    let blocked = creations.draft.as_ref().is_some_and(|d| d.blocked);
    for (entity, transform, creation, is_draft) in &query {
        let color = if is_draft && blocked {
            Color::srgba(1.0, 0.3, 0.25, pulse)
        } else if is_draft {
            Color::srgba(1.0, 0.85, 0.3, pulse)
        } else if creations.selected == Some(entity) {
            Color::srgb(0.4, 0.9, 1.0)
        } else {
            continue;
        };
        let center = transform.translation + transform.rotation * Vec3::Y * (creation.size.y / 2.0);
        let frame = Transform::from_translation(center)
            .with_rotation(transform.rotation)
            .with_scale(creation.size + 0.05);
        gizmos.cube(frame, color);
    }
}

/// Small adjustments to the draft while it follows the cursor.
pub enum Adjust {
    /// Quarter turn; +1 is counter-clockwise seen from above.
    Turn(f32),
    /// Steps of one build voxel above the surface under the cursor.
    Raise(f32),
}

impl Creations {
    pub fn adjust_draft(&mut self, transform: &mut Transform, how: Adjust) {
        let Some(draft) = &mut self.draft else {
            return;
        };
        match how {
            Adjust::Turn(sign) => transform.rotate_y(sign * FRAC_PI_2),
            Adjust::Raise(sign) => {
                let lift = (draft.lift + sign * VOXEL_SIZE).max(0.0);
                transform.translation.y += lift - draft.lift;
                draft.lift = lift;
            }
        }
    }
}

/// Top of the highest solid terrain voxel at or below `at`.
fn ground_below(world: &VoxelWorld, at: Vec3) -> Option<f32> {
    let start = VoxelWorld::voxel_at(at);
    (0..64).map(|dy| start - IVec3::Y * dy).find_map(|p| {
        world.get(p).is_some_and(is_solid).then(|| (p.y + 1) as f32 * crate::world::terrain::VOXEL_SIZE)
    })
}

/// Where a draft should stand for a cursor ray hitting `point` on a face with `normal`,
/// Minecraft-style: on top of upward faces, against side faces (resting on the ground),
/// never under ceilings. `size` is the draft's world-space extent; the result is its
/// bottom-center, snapped to the build grid.
pub fn placement_for_hit(world: &VoxelWorld, point: Vec3, normal: Vec3, size: Vec3) -> Option<Vec3> {
    let mut at = if normal.y > 0.5 {
        point
    } else if normal.y < -0.5 {
        return None;
    } else {
        // Push the footprint out of the wall, then let it rest on the ground there.
        let out = Vec3::new(normal.x, 0.0, normal.z).normalize_or_zero();
        let half = (out * size / 2.0).abs().element_sum();
        let mut at = point + out * (half + 0.01);
        at.y = ground_below(world, Vec3::new(at.x, point.y + 0.1, at.z))?;
        at
    };
    at.x = (at.x / VOXEL_SIZE).round() * VOXEL_SIZE;
    at.z = (at.z / VOXEL_SIZE).round() * VOXEL_SIZE;
    Some(at)
}
