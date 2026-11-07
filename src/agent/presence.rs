//! Seeing each other, like Figma's live cursors: where other players are, what they've
//! selected, the ghost of what they're drafting, and whether their agent is working.
//! Presence is sent when it changes and never kept in the log.
//!
//! Also, offline or on: hovering a spot or a build for a moment says who last changed it.

use bevy::{platform::collections::HashMap, prelude::*, window::PrimaryWindow};
use bevy_egui::{EguiContexts, egui, input::EguiWantsInput};
use cubelark_net::{Presence, ToServer};
use cubelark_rules::{
    log::{Action, fnv1a},
    sculpt::{Area, surface},
};

use super::{
    Chat, cursor_ray,
    creations::{Creation, Creations, DraftMarker},
    landscape::{AreaGizmos, Landscape, perimeter},
    pick,
};
use crate::{
    player::Player,
    voxel::{AIR, VoxelModel, VoxelModelSource},
    world::{
        VoxelWorld,
        ledger::Ledger,
        terrain::{VOXEL_SIZE, WORLD_HEIGHT},
    },
};

/// Fewest seconds between presence updates.
const SEND_EVERY: f32 = 0.1;

pub struct Peer {
    pub name: String,
    pub presence: Presence,
    pub color: Color,
    avatar: Entity,
}

/// Everyone else on the server, by player id.
#[derive(Resource, Default)]
pub struct Peers(pub HashMap<String, Peer>);

impl Peers {
    pub fn seen(&mut self, commands: &mut Commands, assets: &AssetServer, player: String, name: String, presence: Presence) {
        if let Some(peer) = self.0.get_mut(&player) {
            peer.name = name;
            peer.presence = presence;
            return;
        }
        let position = Vec3::from_array(presence.position);
        let avatar = commands
            .spawn((
                Name::new(format!("Player: {name}")),
                VoxelModelSource(assets.load("characters/hero.vox.ron")),
                Transform::from_translation(position).with_rotation(Quat::from_rotation_y(presence.yaw)),
            ))
            .id();
        let color = color_of(&player);
        self.0.insert(player, Peer { name, presence, color, avatar });
    }

    pub fn gone(&mut self, commands: &mut Commands, player: &str) {
        if let Some(peer) = self.0.remove(player) {
            commands.entity(peer.avatar).despawn();
        }
    }

    pub fn clear(&mut self, commands: &mut Commands) {
        for (_, peer) in self.0.drain() {
            commands.entity(peer.avatar).despawn();
        }
    }
}

/// Each player's own color, from their id.
pub fn color_of(player: &str) -> Color {
    Color::hsl((fnv1a(player.as_bytes()) % 360) as f32, 0.8, 0.62)
}

/// Sends our presence when it changes (at most ten times a second).
pub fn send_presence(
    time: Res<Time>,
    ledger: Res<Ledger>,
    chat: Res<Chat>,
    creations: Res<Creations>,
    landscape: Res<Landscape>,
    players: Query<&Transform, With<Player>>,
    drafts: Query<(&Transform, &Creation), With<DraftMarker>>,
    mut last: Local<(f32, Option<Presence>)>,
) {
    let Some(online) = ledger.online.as_ref().filter(|o| o.closed.is_none() && o.state.is_some()) else {
        *last = default();
        return;
    };
    let Ok(hero) = players.single() else { return };
    if time.elapsed_secs() - last.0 < SEND_EVERY {
        return;
    }
    // Rounded, so standing still doesn't count as a change.
    let round = |v: f32| (v * 20.0).round() / 20.0;
    let draft = creations.draft.as_ref().and_then(|d| drafts.get(d.entity).ok()).map(|(t, c)| {
        (t.translation.to_array().map(round), round(t.rotation.to_euler(EulerRot::YXZ).0), c.size.to_array())
    });
    let selected =
        creations.selected.and_then(|e| creations.placed.iter().find(|(entity, _)| *entity == e)).map(|(_, c)| c.build);
    let presence = Presence {
        position: hero.translation.to_array().map(round),
        yaw: round(hero.rotation.to_euler(EulerRot::YXZ).0),
        area: landscape.target(),
        previewing: landscape.draft.is_some(),
        draft,
        selected,
        status: chat.pending.as_ref().map(|p| format!("{} is designing…", p.provider.label())),
    };
    if last.1.as_ref() != Some(&presence) {
        online.client.send(ToServer::Presence(presence.clone()));
        *last = (time.elapsed_secs(), Some(presence));
    }
}

/// Moves avatars smoothly toward where their players are.
pub fn move_avatars(time: Res<Time>, peers: Res<Peers>, mut transforms: Query<&mut Transform>) {
    let blend = 1.0 - (-12.0 * time.delta_secs()).exp();
    for peer in peers.0.values() {
        let Ok(mut transform) = transforms.get_mut(peer.avatar) else { continue };
        let goal = Vec3::from_array(peer.presence.position);
        if transform.translation.distance(goal) > 8.0 {
            transform.translation = goal;
        } else {
            transform.translation = transform.translation.lerp(goal, blend);
        }
        let turn = Quat::from_rotation_y(peer.presence.yaw);
        transform.rotation = transform.rotation.slerp(turn, blend);
    }
}

/// Top of the ground (or water) at a world column, in meters.
fn ground_at(world: &VoxelWorld, column: IVec2) -> Option<f32> {
    let mut voxels = vec![AIR; WORLD_HEIGHT as usize];
    world.column(column.x, column.y, &mut voxels).then(|| {
        let (g, w, _) = surface(&voxels);
        (g.max(w) + 1) as f32 * VOXEL_SIZE + 0.1
    })
}

/// An area's outline along the ground, every few voxels.
fn area_outline(world: &VoxelWorld, area: Area) -> Vec<Vec3> {
    perimeter(area, 4)
        .into_iter()
        .filter_map(|c| ground_at(world, c).map(|y| Vec3::new(c.x as f32, 0.0, c.y as f32) * VOXEL_SIZE + Vec3::Y * y))
        .collect()
}

/// The ring under each other player: thinner than area outlines, so a small circle
/// stays round instead of turning into a blob.
#[derive(Default, Reflect, GizmoConfigGroup)]
pub struct RingGizmos;

pub fn configure_ring_gizmos(mut store: ResMut<GizmoConfigStore>) {
    let (config, _) = store.config_mut::<RingGizmos>();
    config.line.width = 5.0;
    config.line.joints = GizmoLineJoint::Round(4);
    config.depth_bias = -0.3;
}

/// Each peer's ring, selected area or build, and draft ghost, in their color.
pub fn draw_peers(
    time: Res<Time>,
    peers: Res<Peers>,
    world: Res<VoxelWorld>,
    creations: Res<Creations>,
    builds: Query<(&Transform, &Creation)>,
    avatars: Query<&Transform, Without<Creation>>,
    mut gizmos: Gizmos<AreaGizmos>,
    mut rings: Gizmos<RingGizmos>,
) {
    let pulse = 0.65 + 0.35 * (time.elapsed_secs() * 3.0).sin();
    for peer in peers.0.values() {
        let p = &peer.presence;
        // Brighter than white, like our own outlines: tonemapping dulls plain colors.
        let color = Color::LinearRgba(peer.color.to_linear() * 2.5);
        // Around the avatar as drawn, which glides between updates, not where the last
        // update put the player.
        let at = avatars.get(peer.avatar).map_or(Vec3::from_array(p.position), |t| t.translation);
        let feet = Isometry3d::new(at + Vec3::Y * 0.06, Quat::from_rotation_x(std::f32::consts::FRAC_PI_2));
        rings.circle(feet, 0.45, color).resolution(64);
        if let Some(area) = p.area {
            let faded = if p.previewing { color.with_alpha(pulse) } else { color };
            gizmos.linestrip(area_outline(&world, area), faded);
        }
        if let Some((at, yaw, size)) = p.draft {
            let size = Vec3::from_array(size);
            let center = Vec3::from_array(at) + Quat::from_rotation_y(yaw) * Vec3::Y * (size.y / 2.0);
            let frame = Transform::from_translation(center).with_rotation(Quat::from_rotation_y(yaw)).with_scale(size);
            gizmos.cube(frame, color.with_alpha(pulse));
        }
        let selected = p.selected.and_then(|b| creations.placed.iter().find(|(_, c)| c.build == b)).map(|(e, _)| *e);
        if let Some((t, c)) = selected.and_then(|e| builds.get(e).ok()) {
            let center = t.translation + t.rotation * Vec3::Y * (c.size.y / 2.0);
            gizmos.cube(Transform::from_translation(center).with_rotation(t.rotation).with_scale(c.size + 0.08), color);
        }
    }
}

/// Name tags (and what their agent is doing) above other players.
pub fn name_tags(
    mut contexts: EguiContexts,
    peers: Res<Peers>,
    cameras: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    avatars: Query<&Transform>,
) -> Result {
    if peers.0.is_empty() {
        return Ok(());
    }
    let Ok((camera, camera_transform)) = cameras.single() else { return Ok(()) };
    let ctx = contexts.ctx_mut()?;
    for (id, peer) in &peers.0 {
        let Ok(t) = avatars.get(peer.avatar) else { continue };
        let head = t.translation + Vec3::Y * 1.9;
        // Only players in front of the camera get a tag.
        if (head - camera_transform.translation()).dot(*camera_transform.forward()) <= 0.0 {
            continue;
        }
        let Ok(at) = camera.world_to_viewport(camera_transform, head) else { continue };
        let [r, g, b, _] = peer.color.to_srgba().to_u8_array();
        egui::Area::new(egui::Id::new(("peer", id)))
            .fixed_pos(egui::pos2(at.x, at.y))
            .pivot(egui::Align2::CENTER_BOTTOM)
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(egui::Color32::from_rgba_unmultiplied(r, g, b, 220))
                    .corner_radius(6.0)
                    .inner_margin(egui::Margin::symmetric(8, 3))
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(&peer.name).color(egui::Color32::BLACK).strong());
                        if let Some(status) = &peer.presence.status {
                            ui.label(egui::RichText::new(status).color(egui::Color32::BLACK).small());
                        }
                    });
            });
    }
    Ok(())
}

/// After the cursor rests on a build or on changed ground, says who changed it last.
pub fn hover_credit(
    mut contexts: EguiContexts,
    time: Res<Time>,
    windows: Query<&Window, With<PrimaryWindow>>,
    cameras: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    egui_input: Res<EguiWantsInput>,
    world: Res<VoxelWorld>,
    ledger: Res<Ledger>,
    creations: Res<Creations>,
    landscape: Res<Landscape>,
    builds: Query<(Entity, &VoxelModel, &GlobalTransform), (With<Creation>, Without<DraftMarker>)>,
    mut rest: Local<(Vec2, f32)>,
) -> Result {
    if creations.draft.is_some() || landscape.selecting || landscape.draft.is_some() {
        return Ok(());
    }
    let Some((cursor, ray)) = cursor_ray(&windows, &cameras, &egui_input) else { return Ok(()) };
    let now = time.elapsed_secs();
    if cursor.distance(rest.0) > 4.0 {
        *rest = (cursor, now);
        return Ok(());
    }
    if now - rest.1 < 0.6 {
        return Ok(());
    }
    let Some((hit, entity)) = pick(ray, &world, &builds) else { return Ok(()) };
    let text = match entity {
        Some(entity) => {
            let Some((_, build)) = creations.placed.iter().find(|(e, _)| *e == entity) else { return Ok(()) };
            let Some((made, changed)) = ledger.credits(build.build) else { return Ok(()) };
            let by = changed.unwrap_or(made);
            let verb = if changed.is_some() { "Changed" } else { "Placed" };
            format!("\"{}\": {verb} by {}", build.name, ledger.describe(by))
        }
        None => {
            let column = VoxelWorld::voxel_at(ray.get_point(hit.distance) - hit.normal * 0.01).xz();
            let last = ledger.log.effective().filter_map(|c| match &c.action {
                Action::Terrain { name, area, .. } if area.contains(column) => Some((c, name)),
                _ => None,
            });
            let Some((change, name)) = last.last() else { return Ok(()) };
            format!("\"{name}\": reshaped by {}", ledger.describe(&change.by))
        }
    };
    let ctx = contexts.ctx_mut()?;
    egui::Area::new("hover_credit".into())
        .fixed_pos(egui::pos2(cursor.x + 14.0, cursor.y + 14.0))
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(egui::Color32::from_black_alpha(190))
                .corner_radius(5.0)
                .inner_margin(egui::Margin::symmetric(8, 4))
                .show(ui, |ui| {
                    ui.label(egui::RichText::new(text).color(egui::Color32::from_gray(235)).small());
                });
        });
    Ok(())
}
