//! Runs the games' player scripts (`*.player.luau`, [`cubewish_rules::logic::frame`])
//! every frame on this player's machine: they steer the hero, pose the games' pieces and
//! hold the camera. Each installed package's scripts load once per version; one that
//! fails stops until its package changes, and its error goes to the agent with the next
//! request, like the world's script errors.

use std::collections::{BTreeMap, HashMap};

use bevy::prelude::*;
use cubewish_rules::logic::{
    EntityId,
    frame::{FrameIn, Hero, Pose, PlayerVm},
};

use super::Chat;
use crate::{
    player::{self, CameraRig, FollowCamera, HeroBody, InputBlocked, Player, ScriptedHero},
    world::{VoxelWorld, ledger::Ledger},
};

/// Each package's player scripts, by package name: its version's hash and its VM (none
/// when it has no player scripts, or they failed).
#[derive(Default)]
pub struct PlayerScripts {
    loaded: BTreeMap<String, (String, Option<PlayerVm>)>,
    /// Messages sent this second, by package: at most [`SENDS_A_SECOND`].
    sent: BTreeMap<String, (u64, u32)>,
}

/// How many messages a package's player scripts may send its game logic a second: each
/// is a change in the world's log.
const SENDS_A_SECOND: u32 = 10;

/// How the player scripts pose pieces this frame, on top of where they stand.
#[derive(Resource, Default)]
pub struct Poses {
    pub now: HashMap<EntityId, Pose>,
    /// Posed last frame: put back if not posed now.
    pub before: Vec<EntityId>,
}

const KEYS: [(KeyCode, &str); 24] = [
    (KeyCode::KeyW, "w"),
    (KeyCode::KeyA, "a"),
    (KeyCode::KeyS, "s"),
    (KeyCode::KeyD, "d"),
    (KeyCode::Space, "space"),
    (KeyCode::ShiftLeft, "shift"),
    (KeyCode::ControlLeft, "ctrl"),
    (KeyCode::KeyQ, "q"),
    (KeyCode::KeyE, "e"),
    (KeyCode::KeyR, "r"),
    (KeyCode::KeyF, "f"),
    (KeyCode::KeyC, "c"),
    (KeyCode::KeyV, "v"),
    (KeyCode::KeyX, "x"),
    (KeyCode::KeyZ, "z"),
    (KeyCode::ArrowUp, "up"),
    (KeyCode::ArrowDown, "down"),
    (KeyCode::ArrowLeft, "left"),
    (KeyCode::ArrowRight, "right"),
    (KeyCode::Digit1, "1"),
    (KeyCode::Digit2, "2"),
    (KeyCode::Digit3, "3"),
    (KeyCode::Digit4, "4"),
    (KeyCode::Digit5, "5"),
];

#[allow(clippy::too_many_arguments)]
pub fn run_player_scripts(
    mut scripts: NonSendMut<PlayerScripts>,
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
    blocked: Res<InputBlocked>,
    body: Res<HeroBody>,
    mut ledger: ResMut<Ledger>,
    world: Res<VoxelWorld>,
    mut chat: ResMut<Chat>,
    mut scripted: ResMut<ScriptedHero>,
    mut poses: ResMut<Poses>,
    heroes: Query<(&Transform, &Player)>,
    cameras: Query<&FollowCamera>,
) {
    *scripted = ScriptedHero::default();
    poses.before = poses.now.keys().copied().collect();
    poses.now.clear();
    let mut messages = Vec::new();
    let logic = &ledger.logic;
    // Load what's new; forget what's gone.
    scripts.loaded.retain(|name, _| logic.installed.contains_key(name));
    for (name, install) in &logic.installed {
        if scripts.loaded.get(name).is_some_and(|(hash, _)| *hash == install.package.hash) {
            continue;
        }
        let vm = match logic.package(install).map(PlayerVm::new) {
            Some(Ok(vm)) => vm,
            Some(Err(why)) => {
                warn!("{name}'s player scripts didn't load: {why}");
                chat.script_errors.push(format!("{name} (player script): {why}"));
                None
            }
            None => None,
        };
        scripts.loaded.insert(name.clone(), (install.package.hash.clone(), vm));
    }
    let Ok((transform, hero)) = heroes.single() else { return };
    let (held, pressed): (Vec<&str>, Vec<&str>) = if blocked.0 {
        (Vec::new(), Vec::new())
    } else {
        let mut held: Vec<&str> = KEYS.iter().filter(|(k, _)| keys.pressed(*k)).map(|(_, n)| *n).collect();
        let mut pressed: Vec<&str> = KEYS.iter().filter(|(k, _)| keys.just_pressed(*k)).map(|(_, n)| *n).collect();
        for (button, name) in [(MouseButton::Left, "mouse_left"), (MouseButton::Right, "mouse_right")] {
            if buttons.pressed(button) {
                held.push(name);
            }
            if buttons.just_pressed(button) {
                pressed.push(name);
            }
        }
        (held, pressed)
    };
    let camera = cameras.single().map_or([0.0, 0.3], |c| [c.yaw(), c.pitch()]);
    // Where the usual controls would take the hero.
    let axis = |a: &str, b: &str| held.contains(&a) as i32 as f32 - held.contains(&b) as i32 as f32;
    let input = Vec2::new(axis("d", "a") + axis("right", "left"), axis("w", "s") + axis("up", "down")).clamp_length_max(1.0);
    let forward = Vec3::new(-camera[0].sin(), 0.0, -camera[0].cos());
    let right = Vec3::new(-forward.z, 0.0, forward.x);
    let speed = if held.contains(&"shift") { player::RUN_SPEED } else { player::WALK_SPEED } * body.speed;
    let walk = (right * input.x + forward * input.y) * speed;
    let pos = transform.translation;
    let in_water = world.get(VoxelWorld::voxel_at(pos + Vec3::Y * 0.4)) == Some(crate::world::terrain::block::WATER);
    let b = logic.state.body(&ledger.profile.id);
    let frame = FrameIn {
        dt: time.delta_secs().min(1.0 / 20.0),
        now: super::game::now_exact(),
        me: &ledger.profile.id,
        hero: Hero { pos: pos.to_array(), velocity: hero.velocity().to_array(), grounded: hero.grounded(), in_water, facing: hero.facing() },
        keys: &held,
        pressed: &pressed,
        walk: [walk.x, walk.z],
        camera,
        body: b,
        gravity: player::GRAVITY * b.gravity,
    };
    let second = super::game::now();
    let scripts = &mut *scripts;
    for (name, (_, vm)) in scripts.loaded.iter_mut() {
        let Some(running) = vm else { continue };
        match running.frame(&frame, &logic.state) {
            Ok(out) => {
                if let Some(v) = out.velocity {
                    scripted.velocity = Some(Vec3::from_array(v));
                }
                if let Some(c) = out.camera {
                    scripted.camera = Some(CameraRig { distance: c.distance, pitch: c.pitch, yaw: c.yaw, height: c.height, shake: c.shake });
                }
                poses.now.extend(out.poses);
                for (message, data) in out.sent {
                    let count = scripts.sent.entry(name.clone()).or_insert((second, 0));
                    if count.0 != second {
                        *count = (second, 0);
                    }
                    count.1 += 1;
                    if count.1 > SENDS_A_SECOND {
                        warn!("{name}'s player script sends more than {SENDS_A_SECOND} messages a second; dropping \"{message}\"");
                        continue;
                    }
                    messages.push(cubewish_rules::logic::Intent::Message { package: name.clone(), name: message, data, at: pos.to_array() });
                }
            }
            Err(why) => {
                warn!("{name}'s player script stopped: {why}");
                chat.script_errors.push(format!("{name} (player script, stopped): {why}"));
                *vm = None;
            }
        }
    }
    for intent in messages {
        match ledger.act(intent) {
            Ok(acted) => {
                for note in &acted.notes {
                    chat.say(super::Role::System, note.clone());
                }
            }
            Err(why) => chat.script_errors.push(format!("on_message: {why}")),
        }
    }
}

/// Shows the posed pieces where they stand plus their pose, and puts back the ones no
/// longer posed.
pub fn pose_pieces(poses: Res<Poses>, ledger: Res<Ledger>, play: Res<super::game::Play>, mut transforms: Query<&mut Transform>) {
    let ids = poses.now.keys().chain(poses.before.iter().filter(|id| !poses.now.contains_key(id)));
    for id in ids {
        let (Some(e), Some((entity, look))) = (ledger.logic.state.get(*id), play.pieces.get(id)) else { continue };
        let Ok(mut transform) = transforms.get_mut(*entity) else { continue };
        let turn = if let super::game::Look::Model { turn, .. } = look { *turn } else { 0 };
        let mut at = super::game::placement(e, turn, look);
        if let Some(pose) = poses.now.get(id) {
            at.translation += Vec3::from_array(pose.offset);
            at.rotation *= Quat::from_rotation_y(pose.turn);
            at.scale *= pose.scale;
        }
        *transform = at;
    }
}
