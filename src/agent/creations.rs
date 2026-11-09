//! Agent-made objects: a draft preview, placed creations, selection, editing, and
//! undo. Every change is recorded in the world's change log ([`Ledger`]).

use std::f32::consts::{FRAC_PI_2, PI};

use bevy::prelude::*;
pub use cubewish_rules::log::BuildState as SavedCreation;
use cubewish_rules::{
    budget::{Footprint, Usage},
    build::BuildLimits,
    log::{Action, parcels_of},
};

use super::spec::{BuildSpec, BuildStats, VOXEL_SIZE, compile};
use crate::{
    voxel::{Coarse, DepthBias, Dormant, ModelData, VoxelModel},
    world::{CHUNK, ChunkLoader, SolidModel, VoxelWorld, ledger::Ledger, lod::VIEW_DISTANCE, terrain::{self, is_solid}},
};

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
    /// Why the land rules don't allow it where it stands, if they don't.
    pub denied: Option<String>,
    /// Following drafts stick to the cursor; pinned ones (revisions of a placed build)
    /// stay put until picked up.
    pub follow: bool,
    /// The agent designed this version (rather than a copy or a move), so placing it
    /// is recorded as the agent's change.
    pub by_agent: bool,
    /// Where it leads, if the agent made it a portal: linked when it's placed.
    pub portal: Option<cubewish_rules::log::PortalTo>,
}

#[derive(Resource, Default)]
pub struct Creations {
    pub placed: Vec<(Entity, SavedCreation)>,
    pub draft: Option<Draft>,
    pub selected: Option<Entity>,
    /// Our changes online the server hasn't answered yet, newest last: shown already,
    /// and taken back if it refuses them.
    history: Vec<Change>,
    next_id: u64,
}

/// A change shown before the server took it, with its reference.
enum Change {
    Placed { build: u64, change: u64 },
    Deleted { before: SavedCreation, change: u64 },
    /// A build was moved or revised; holds how it was before.
    Replaced { before: SavedCreation, change: u64 },
}

impl Change {
    /// The change-log reference (online: the request) it belongs to.
    fn reference(&self) -> u64 {
        match self {
            Change::Placed { change, .. }
            | Change::Deleted { change, .. }
            | Change::Replaced { change, .. } => *change,
        }
    }
}


/// Marks a creation entity; drafts also carry [`DraftMarker`].
#[derive(Component)]
pub struct Creation {
    pub size: Vec3,
}

#[derive(Component)]
pub struct DraftMarker;

/// Spawns a placed (solid) creation.
fn spawn_placed(commands: &mut Commands, model: &ModelData, saved: &SavedCreation) -> Entity {
    let entity = spawn(commands, model, &saved.name, saved.position.into(), saved.yaw);
    commands.entity(entity).insert(SolidModel);
    entity
}

fn spawn(commands: &mut Commands, model: &ModelData, name: &str, position: Vec3, yaw: f32) -> Entity {
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
        model: &ModelData,
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
        self.draft = Some(Draft { entity, spec, stats, prompt, provider, replaces, lift, blocked: false, denied: None, follow, by_agent: true, portal: None });
    }

    /// Drops the draft; if it was an edit, the original build comes back unchanged.
    pub fn discard_draft(&mut self, commands: &mut Commands) {
        let Some(draft) = self.draft.take() else {
            return;
        };
        commands.entity(draft.entity).despawn();
        if let Some(original) = draft.replaces
            && let Ok(build) = compile(&original.spec, &BuildLimits::ACCEPTED)
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
        let stats = compile(&self.placed[index].1.spec, &BuildLimits::ACCEPTED).ok()?.stats;
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
            denied: None,
            follow,
            by_agent: false,
            portal: None,
        });
        Some(name)
    }

    /// Removes the selected build if the land rules allow it; returns its name, or why not.
    pub fn delete_selected(&mut self, commands: &mut Commands, ledger: &mut Ledger) -> Result<String, String> {
        let entity = self.selected.ok_or("nothing is selected")?;
        let index = self.placed.iter().position(|(e, _)| *e == entity).ok_or("nothing is selected")?;
        ledger
            .check(&parcels_of(&self.placed[index].1))
            .and_then(|_| ledger.check_rate(0))
            .map_err(|why| format!("Can't delete it: {why}."))?;
        self.selected = None;
        let (_, saved) = self.placed.remove(index);
        commands.entity(entity).despawn();
        let name = saved.name.clone();
        let change = ledger.record(None, None, Action::RemoveBuild { build: saved.build });
        self.history.push(Change::Deleted { before: saved, change });
        Ok(name)
    }

    /// Whether the land rules allow the draft at `transform` (a moved or revised build
    /// also needs rights where it stood).
    pub fn check_draft(&self, transform: &Transform, ledger: &Ledger) -> Option<String> {
        let draft = self.draft.as_ref()?;
        let yaw = transform.rotation.to_euler(EulerRot::YXZ).0;
        let here = cubewish_rules::land::build_parcels(transform.translation, yaw, &draft.spec.size);
        let mut parcels = here.clone();
        parcels.extend(draft.replaces.as_ref().map(parcels_of).unwrap_or_default());
        if let Err(why) = ledger.check(&parcels) {
            return Some(why);
        }
        let old = draft.replaces.as_ref().and_then(|r| ledger.footprints.get(&r.build));
        let s = draft.stats;
        let new = Footprint {
            owner: old.map_or(ledger.profile.id.clone(), |o| o.owner.clone()),
            parcels: here,
            usage: Usage { builds: 1, voxels: s.voxels, faces: s.faces, glow: s.glow, terrain: 0 },
        };
        ledger.budgets.check(&new, old, &ledger.rules.budgets).and_then(|_| ledger.check_rate(s.voxels)).err()
    }

    /// Starts a new following draft that copies a placed build (Minecraft's pick block).
    pub fn copy_as_draft(&mut self, commands: &mut Commands, entity: Entity, at: &Transform) -> Option<String> {
        if self.draft.is_some() {
            return None;
        }
        let saved = self.placed.iter().find(|(e, _)| *e == entity).map(|(_, c)| c.clone())?;
        let build = compile(&saved.spec, &BuildLimits::ACCEPTED).ok()?;
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
            denied: None,
            follow: true,
            by_agent: false,
            portal: None,
        });
        Some(saved.name)
    }

    /// Makes the placed builds match `builds` (the change log's), after the log changed
    /// in a way local undo can't express. Undo history no longer applies after that.
    pub fn resync(
        &mut self,
        commands: &mut Commands,
        builds: &std::collections::BTreeMap<u64, SavedCreation>,
    ) {
        let stale: Vec<u64> =
            self.placed.iter().filter(|(_, c)| builds.get(&c.build) != Some(c)).map(|(_, c)| c.build).collect();
        for build in stale {
            self.take_placed(commands, build);
        }
        for c in builds.values() {
            if !self.placed.iter().any(|(_, p)| p.build == c.build) {
                self.restore(commands, c.clone());
            }
        }
        self.history.clear();
    }

    /// Shows a build someone else placed or changed (online).
    pub fn upsert(&mut self, commands: &mut Commands, build: &SavedCreation) {
        if self.draft.as_ref().and_then(|d| d.replaces.as_ref()).is_some_and(|r| r.build == build.build) {
            // Being edited here; whichever is placed last wins.
            return;
        }
        self.take_placed(commands, build.build);
        self.restore(commands, build.clone());
    }

    /// Removes a build someone else removed (online).
    pub fn remove(&mut self, commands: &mut Commands, build: u64) {
        self.take_placed(commands, build);
    }

    /// Takes back a change of ours the server refused (by its reference): what was
    /// shown goes back to how it was, and it leaves the undo history. Terrain changes
    /// are the landscape's to take back.
    pub fn retract(&mut self, commands: &mut Commands, reference: u64) {
        let Some(index) = self.history.iter().position(|c| c.reference() == reference) else {
            return;
        };
        match self.history.remove(index) {
            Change::Placed { build, .. } => {
                self.take_placed(commands, build);
            }
            Change::Deleted { before, .. } => {
                self.restore(commands, before);
            }
            Change::Replaced { before, .. } => {
                self.take_placed(commands, before.build);
                self.restore(commands, before);
            }
        }
    }

    pub fn selected(&self) -> Option<&SavedCreation> {
        let entity = self.selected?;
        self.placed.iter().find(|(e, _)| *e == entity).map(|(_, c)| c)
    }

    /// Makes the draft permanent where it stands (`transform` is the draft's). Returns
    /// its name, or why it can't be placed there.
    pub fn place_draft(&mut self, commands: &mut Commands, transform: &Transform, ledger: &mut Ledger) -> Result<String, String> {
        let draft = self.draft.as_ref().ok_or("there's no draft")?;
        if draft.blocked {
            return Err("Can't place it on top of you; point somewhere else.".into());
        }
        if let Some(why) = self.check_draft(transform, ledger) {
            return Err(format!("Can't place it here: {why}."));
        }
        let draft = self.draft.take().expect("checked above");
        commands.entity(draft.entity).remove::<DraftMarker>().insert(SolidModel);
        let known = self.placed.iter().map(|(_, c)| c.build).chain(ledger.builds.keys().copied());
        self.next_id = self.next_id.max(known.max().map_or(1, |b| b + 1));
        // An edit keeps its original id. Online, others pick ids at the same time, so a
        // new build takes a random one instead of the next.
        let build = draft.replaces.as_ref().map(|r| r.build).unwrap_or_else(|| {
            if ledger.is_online() {
                let mut bytes = [0u8; 8];
                let _ = getrandom::fill(&mut bytes);
                // Kept below 2^53 so it survives JSON readers that use doubles.
                return u64::from_le_bytes(bytes) >> 11;
            }
            self.next_id += 1;
            self.next_id - 1
        });
        let saved = SavedCreation {
            build,
            name: draft.spec.name.clone(),
            prompt: draft.prompt.clone(),
            provider: draft.provider.clone(),
            position: transform.translation.into(),
            yaw: transform.rotation.to_euler(EulerRot::YXZ).0,
            spec: draft.spec,
        };
        let name = saved.name.clone();
        let (agent, prompt) = match draft.by_agent {
            true => (Some(draft.provider.as_str()), Some(draft.prompt.as_str())),
            false => (None, None),
        };
        let action = match draft.replaces {
            Some(_) => Action::UpdateBuild(saved.clone()),
            None => Action::PlaceBuild(saved.clone()),
        };
        let change = ledger.record(agent, prompt, action);
        if let Some(to) = draft.portal {
            ledger.record(None, None, Action::SetPortal { build, to: Some(to) });
        }
        self.history.push(match draft.replaces {
            Some(before) => Change::Replaced { before, change },
            None => Change::Placed { build, change },
        });
        self.placed.push((draft.entity, saved));
        // Stay selected so a follow-up chat message ("make it blue") applies to it.
        self.selected = Some(draft.entity);
        Ok(name)
    }

    fn take_placed(&mut self, commands: &mut Commands, build: u64) -> Option<(Entity, SavedCreation)> {
        let index = self.placed.iter().position(|(_, c)| c.build == build)?;
        let (entity, saved) = self.placed.remove(index);
        if self.selected == Some(entity) {
            self.selected = None;
        }
        commands.entity(entity).despawn();
        Some((entity, saved))
    }

    fn restore(&mut self, commands: &mut Commands, saved: SavedCreation) -> Option<Entity> {
        let build = compile(&saved.spec, &BuildLimits::ACCEPTED).ok()?;
        let entity = spawn_placed(commands, &build.model, &saved);
        self.placed.push((entity, saved));
        Some(entity)
    }
}

/// Spawns the builds the change log describes.
pub fn load_creations(mut commands: Commands, mut creations: ResMut<Creations>, ledger: Res<Ledger>) {
    for c in ledger.builds.values() {
        // Saved specs are re-validated, so tightened limits apply to old builds too.
        match compile(&c.spec, &BuildLimits::ACCEPTED) {
            Ok(build) => {
                // Asleep until the hero comes near; see `stream_creations`.
                let entity = spawn_placed(&mut commands, &build.model, c);
                commands.entity(entity).insert(Dormant);
                creations.placed.push((entity, c.clone()));
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
    let blocked = creations.draft.as_ref().is_some_and(|d| d.blocked || d.denied.is_some());
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

/// Builds this close (plus their own size) are drawn in full detail; farther ones
/// at half resolution, which their small voxels hide at that distance.
const DETAIL_DISTANCE: f32 = 40.0;

/// Builds out of view go [`Dormant`] (no meshes or lights) and wake as the hero comes
/// near, like terrain chunks, and far ones are drawn [`Coarse`], so drawing them costs
/// what is in view, and less the farther it is. Margins keep a build at an edge from
/// switching back and forth.
pub fn stream_creations(
    mut commands: Commands,
    loaders: Query<&GlobalTransform, With<ChunkLoader>>,
    builds: Query<(Entity, &Transform, &Creation, Has<Dormant>, Has<Coarse>), Without<DraftMarker>>,
) {
    // Terrain voxels, which are bigger than a build's.
    let margin = (2 * CHUNK) as f32 * terrain::VOXEL_SIZE;
    for (entity, transform, creation, dormant, coarse) in &builds {
        let distance = loaders
            .iter()
            .map(|l| (l.translation() - transform.translation).xz().length())
            .fold(f32::INFINITY, f32::min);
        // However the build sits around its origin, it's within its diagonal of it.
        let size = creation.size.length();
        let mut e = commands.entity(entity);
        if dormant && distance <= VIEW_DISTANCE + size {
            e.remove::<Dormant>();
        } else if !dormant && distance > VIEW_DISTANCE + size + margin {
            e.insert(Dormant);
        }
        if coarse && distance <= DETAIL_DISTANCE + size {
            e.remove::<Coarse>();
        } else if !coarse && distance > DETAIL_DISTANCE + size + margin / 2.0 {
            e.insert(Coarse);
        }
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;

    #[test]
    fn far_builds_sleep_and_near_ones_wake() {
        let mut app = App::new();
        app.add_systems(Update, stream_creations);
        app.world_mut().spawn((ChunkLoader, GlobalTransform::default()));
        let size = Vec3::splat(4.0);
        let far = app.world_mut().spawn((Creation { size }, Transform::from_xyz(200.0, 0.0, 0.0))).id();
        let near = app.world_mut().spawn((Creation { size }, Transform::from_xyz(10.0, 0.0, 0.0), Dormant)).id();
        // Just past the view, inside the margin: stays as it is.
        let edge = app.world_mut().spawn((Creation { size }, Transform::from_xyz(VIEW_DISTANCE + 10.0, 0.0, 0.0))).id();
        let draft = app.world_mut().spawn((Creation { size }, DraftMarker, Transform::from_xyz(300.0, 0.0, 0.0))).id();
        app.update();
        let world = app.world();
        assert!(world.get::<Dormant>(far).is_some());
        assert!(world.get::<Dormant>(near).is_none());
        assert!(world.get::<Dormant>(edge).is_none());
        assert!(world.get::<Dormant>(draft).is_none());
        // Far ones are coarse, near ones full detail.
        assert!(world.get::<Coarse>(far).is_some());
        assert!(world.get::<Coarse>(near).is_none());
        assert!(world.get::<Coarse>(edge).is_some());
    }
}
