//! Agent-made objects: a draft preview, placed creations, undo, and saving to disk.

use std::f32::consts::{FRAC_PI_2, PI};

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use super::spec::{BuildSpec, BuildStats, VOXEL_SIZE, compile};
use crate::{
    voxel::{VoxelModel, VoxelModelAsset},
    world::{VoxelWorld, terrain::is_solid},
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
}

#[derive(Resource, Default)]
pub struct Creations {
    pub placed: Vec<(Entity, SavedCreation)>,
    pub draft: Option<Draft>,
    next_id: u64,
}

/// Marks a creation entity; drafts also carry [`DraftMarker`].
#[derive(Component)]
pub struct Creation {
    pub size: Vec3,
}

#[derive(Component)]
pub struct DraftMarker;

fn spawn(commands: &mut Commands, model: &VoxelModelAsset, name: &str, position: Vec3, yaw: f32) -> Entity {
    let size = model.grid.size().as_vec3() * VOXEL_SIZE;
    commands
        .spawn((
            Name::new(format!("Creation: {name}")),
            Creation { size },
            VoxelModel::from(model),
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
    pub fn set_draft(
        &mut self,
        commands: &mut Commands,
        model: &VoxelModelAsset,
        draft: (BuildSpec, BuildStats, String, String),
        at: (Vec3, f32),
    ) {
        self.discard_draft(commands);
        let (spec, stats, prompt, provider) = draft;
        let entity = spawn(commands, model, &spec.name, at.0, at.1);
        commands.entity(entity).insert(DraftMarker);
        self.draft = Some(Draft { entity, spec, stats, prompt, provider });
    }

    pub fn discard_draft(&mut self, commands: &mut Commands) {
        if let Some(draft) = self.draft.take() {
            commands.entity(draft.entity).despawn();
        }
    }

    /// Makes the draft permanent where it stands (`transform` is the draft's).
    pub fn place_draft(&mut self, commands: &mut Commands, transform: &Transform) -> Option<String> {
        let draft = self.draft.take()?;
        commands.entity(draft.entity).remove::<DraftMarker>();
        self.next_id = self.next_id.max(self.placed.iter().map(|(_, c)| c.id + 1).max().unwrap_or(0));
        let saved = SavedCreation {
            id: self.next_id,
            name: draft.spec.name.clone(),
            prompt: draft.prompt,
            provider: draft.provider,
            position: transform.translation.into(),
            yaw: transform.rotation.to_euler(EulerRot::YXZ).0,
            spec: draft.spec,
        };
        self.next_id += 1;
        let name = saved.name.clone();
        self.placed.push((draft.entity, saved));
        self.save();
        Some(name)
    }

    pub fn undo(&mut self, commands: &mut Commands) -> Option<String> {
        let (entity, saved) = self.placed.pop()?;
        commands.entity(entity).despawn();
        self.save();
        Some(saved.name)
    }

    fn save(&self) {
        let list: Vec<_> = self.placed.iter().map(|(_, c)| c).collect();
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
                let entity = spawn(&mut commands, &build.model, &c.name, c.position.into(), c.yaw);
                creations.placed.push((entity, c));
            }
            Err(errors) => warn!("skipping saved creation {:?}: {}", c.name, errors.join("; ")),
        }
    }
}

/// Outlines the draft so it reads as "not placed yet".
pub fn outline_draft(time: Res<Time>, mut gizmos: Gizmos, drafts: Query<(&Transform, &Creation), With<DraftMarker>>) {
    let pulse = 0.6 + 0.4 * (time.elapsed_secs() * 3.0).sin();
    for (transform, creation) in &drafts {
        let center = transform.translation + transform.rotation * Vec3::Y * (creation.size.y / 2.0);
        let frame = Transform::from_translation(center)
            .with_rotation(transform.rotation)
            .with_scale(creation.size + 0.05);
        gizmos.cube(frame, Color::srgba(1.0, 0.85, 0.3, pulse));
    }
}
