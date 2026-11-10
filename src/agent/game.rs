//! Playing the games installed in the world (`genverse_rules::logic`).
//!
//! - **Pieces:** every entity the game rules keep is drawn from its `look`: one of its
//!   package's voxel models, drawn like a build. Pieces with a `label` show it when
//!   you're within 10 m; the one in reach says E uses it, and F attacks it (`on_hit`).
//! - The hero goes where games send them: a teleport, and on coming into a world, the
//!   game's spawn point.
//! - What you carry is the [inventory](super::inventory)'s: a hotbar, and right-click
//!   to put down or use what's in hand.
//! - **Keys:** when a game gives you a `pawn` in one of its regions that takes keys
//!   (`keys = true`), your hero stands on it and the movement keys go to the game as
//!   world directions (WASD or arrows, turned to the camera: W is away from it), with R
//!   to reset, E for action and Esc to leave.
//! - What the rules say back ("Solved!") shows at the top of the view. Script errors also
//!   go to the chat, and to the agent with the next request, so asking it to fix them
//!   is enough.
//! - **Timers:** offline, the game runs the packages' timers twice a second (online, the
//!   server does), and shows what they say to you the same way.
//!
//! Everything goes through the ledger as intents, tried first so a refusal shows right
//! away instead of landing in the log.
//!
//! Script it: `GENVERSE_PLAY="sokoban:board; go 40 40; east; east"` puts things down in
//! front of the hero (`place` puts down what's in hand), uses the nearest piece
//! (`use`), goes through the portal the hero stands at (`portal`) or sends a key to the
//! region taking keys the hero is in, one step every half second once
//! the hero has landed and the agent is done with `GENVERSE_PROMPT`.

use std::collections::HashMap;

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};
use std::f32::consts::FRAC_PI_2;

use genverse_rules::{
    build::{BuildLimits, compile},
    logic::{
        Entity as Piece, EntityId, Intent, Logic, Package,
        data,
        clockwork::keys_region_at,
        walk::{position_at, walking_at},
    },
};

use super::{Chat, Role, creations::Creations, landscape::Landscape, theme};
use crate::{
    player::{FollowCamera, HeroBody, InputBlocked, Pawned, Player},
    voxel::{DepthBias, ModelData, VoxelModel},
    world::{
        SolidModel, VoxelWorld,
        ledger::{Acted, Ledger},
    },
};

/// How close the hero must be to use a piece, in meters.
const REACH: f32 = 2.5;
/// Pieces this close get a name label, so they're easy to find.
const LABEL_DISTANCE: f32 = 10.0;
const TOAST_SECONDS: f32 = 3.0;
/// A held direction key repeats after this long, this often.
const REPEAT_AFTER: f32 = 0.35;
const REPEAT_EVERY: f32 = 0.16;

#[derive(Resource, Default)]
pub struct Play {
    /// The piece E would use: the nearest labeled one within reach.
    pub near: Option<EntityId>,
    /// The region taking keys where the local player's pawn is: it gets the movement keys.
    pub playing: Option<EntityId>,
    /// The direction key held, since when, and when it last repeated.
    held: Option<(KeyCode, f32, f32)>,
    /// A short note at the top of the view, and when it appeared.
    toast: Option<(String, f32)>,
    /// The pieces in the world and how each looks.
    pub(super) pieces: HashMap<EntityId, (Entity, Look)>,
    models: Models,
}

impl Play {
    pub fn toast(&mut self, text: impl Into<String>, now: f32) {
        self.toast = Some((text.into(), now));
    }

    /// Forgets what was going on in the world left behind: playing, near.
    pub fn leave_world(&mut self) {
        self.near = None;
        self.playing = None;
        self.held = None;
    }
}

/// How a piece is drawn.
#[derive(Clone, PartialEq)]
pub(super) enum Look {
    /// One of its package's voxel models (by package hash and name), turned by quarter
    /// turns.
    Model { package: String, model: String, turn: u8, scale: f32, tint: Option<String>, glow: bool },
    /// Nothing to draw (a pawn: the hero shows it).
    Hidden,
    /// Its package isn't installed (any more): kept, but asleep.
    Dormant,
}

impl Look {
    /// A piece with no model of its own (its game is gone): a box's size and color.
    fn block(&self) -> Option<(Vec3, Color)> {
        Some(match self {
            Look::Dormant => (Vec3::splat(0.2), Color::srgb(0.5, 0.5, 0.5)),
            Look::Model { .. } | Look::Hidden => return None,
        })
    }
}

/// Compiled voxel models, by package hash, model name, tint and glow (none if it
/// doesn't compile).
#[derive(Default)]
struct Models(HashMap<(String, String, Option<String>, bool), Option<ModelData>>);

impl Models {
    fn get(&mut self, logic: &Logic, package: &str, model: &str) -> Option<&ModelData> {
        self.tinted(logic, package, model, None, false)
    }

    /// The model with its colors multiplied by `tint`, and all of it glowing if `glow`.
    fn tinted(&mut self, logic: &Logic, package: &str, model: &str, tint: Option<&str>, glow: bool) -> Option<&ModelData> {
        self.0
            .entry((package.to_string(), model.to_string(), tint.map(str::to_string), glow))
            .or_insert_with(|| {
                let mut spec = logic.packages.get(package)?.models.get(model)?.clone();
                let tint = tint.and_then(|t| Srgba::hex(t.trim_start_matches('#')).ok());
                for color in &mut spec.palette {
                    if let (Some(tint), Ok(c)) = (tint, Srgba::hex(color.hex.trim_start_matches('#'))) {
                        color.hex = Srgba::rgb(c.red * tint.red, c.green * tint.green, c.blue * tint.blue).to_hex();
                    }
                    if glow && color.material != "glass" {
                        color.material = "glow".into();
                    }
                }
                compile(&spec, &BuildLimits::ACCEPTED).map(|b| b.model).ok()
            })
            .as_ref()
    }

    /// How tall a piece stands, in meters.
    fn height(&mut self, logic: &Logic, look: &Look) -> f32 {
        match look {
            Look::Model { package, model, scale, .. } => self.get(logic, package, model).map_or(0.0, |m| m.grid.size().y as f32 * m.voxel_size * scale),
            other => other.block().map_or(0.0, |(size, _)| size.y),
        }
    }
}

/// Package `package`, if it's installed.
fn package_of<'a>(logic: &'a Logic, package: &str) -> Option<&'a Package> {
    logic.installed.get(package).and_then(|i| logic.package(i))
}

/// How a piece looks and what it's called: its `look` and `label`.
fn describe(logic: &Logic, e: &Piece) -> (Look, Option<String>) {
    let label = e.components.get("label").and_then(|l| l.as_str()).map(str::to_string);
    if package_of(logic, &e.package).is_none() {
        return (Look::Dormant, Some(format!("{} (not installed)", e.package)));
    }
    match e.components.get("look") {
        Some(look) if look["hidden"] == true => (Look::Hidden, label),
        Some(look) => {
            let installed = logic.installed.get(&e.package).map(|i| i.package.hash.clone()).unwrap_or_default();
            let model = look["model"].as_str().unwrap_or_default().to_string();
            let turn = look["turn"].as_u64().unwrap_or(0).min(3) as u8;
            let scale = look["scale"].as_f64().unwrap_or(1.0).clamp(0.1, 8.0) as f32;
            let tint = look["tint"].as_str().map(str::to_string);
            (Look::Model { package: installed, model, turn, scale, tint, glow: look["glow"] == true }, label)
        }
        None => (Look::Hidden, label),
    }
}

pub(super) fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Now, to the millisecond: where walking pieces are.
pub(super) fn now_exact() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// Where a piece stands now and how it's turned: along its walk, facing the way it
/// goes, if it's walking.
pub(super) fn placement(e: &Piece, turn: u8, look: &Look) -> Transform {
    let (at, yaw) = walking_at(e, now_exact()).unwrap_or((e.position, turn as f32 * FRAC_PI_2));
    let mut transform = Transform::from_translation(Vec3::from_array(at)).with_rotation(Quat::from_rotation_y(yaw));
    if let Some((size, _)) = look.block() {
        transform.translation.y += size.y / 2.0;
    }
    if let Look::Model { scale, .. } = look {
        transform.scale = Vec3::splat(*scale);
    }
    transform
}

/// Moves the walking pieces along their way every frame: the log holds only where each
/// walk goes and when it set off.
pub fn walk_pieces(ledger: Res<Ledger>, play: Res<Play>, mut transforms: Query<&mut Transform>) {
    for (id, e) in ledger.logic.state.entities() {
        if !e.components.contains_key("walk") {
            continue;
        }
        let Some((entity, look)) = play.pieces.get(&id) else { continue };
        let turn = if let Look::Model { turn, .. } = look { *turn } else { 0 };
        if let Ok(mut transform) = transforms.get_mut(*entity) {
            *transform = placement(e, turn, look);
        }
    }
}

/// Keeps a piece in the world for each of the game's entities: its voxel model, drawn
/// like a build, moved when it moves and rebuilt when its look changes (and every couple
/// of seconds, for crops that grow with the date).
pub fn sync_pieces(
    mut commands: Commands,
    time: Res<Time>,
    ledger: Res<Ledger>,
    mut play: ResMut<Play>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut seen: Local<(u64, f32)>,
) {
    let now_s = time.elapsed_secs();
    if seen.0 == ledger.version && now_s - seen.1 < 2.0 {
        return;
    }
    *seen = (ledger.version, now_s);
    let logic = &ledger.logic;
    let play = &mut *play;
    let mut keep = HashMap::new();
    for (id, e) in logic.state.entities() {
        let (look, _) = describe(logic, e);
        let turn = if let Look::Model { turn, .. } = look { turn } else { 0 };
        let mut transform = placement(e, turn, &Look::Hidden);
        // Solid while it stands still: the hero bumps into it as into a build.
        let solid = e.components.get("look").is_some_and(|l| l["solid"] == true)
            && !e.components.contains_key("walk")
            && !e.components.contains_key("chase");
        if let Some((entity, old)) = play.pieces.remove(&id) {
            if old == look {
                let transform = placement(e, turn, &look);
                commands.entity(entity).insert(transform);
                if solid {
                    commands.entity(entity).insert(SolidModel);
                } else {
                    commands.entity(entity).remove::<SolidModel>();
                }
                keep.insert(id, (entity, look));
                continue;
            }
            commands.entity(entity).despawn();
        }
        let mut entity = commands.spawn((Visibility::default(), Name::new("Game piece")));
        match &look {
            Look::Model { package, model, tint, glow, .. } => {
                if let Some(data) = play.models.tinted(logic, package, model, tint.as_deref(), *glow) {
                    // Pieces stand on the ground, so their faces often meet it, as builds' do.
                    entity.insert((VoxelModel::from(data), DepthBias(500.0)));
                    if solid {
                        entity.insert(SolidModel);
                    }
                }
            }
            other => {
                if let Some((size, color)) = other.block() {
                    transform.translation.y += size.y / 2.0;
                    entity.insert((
                        Mesh3d(meshes.add(Cuboid::from_size(size))),
                        MeshMaterial3d(materials.add(StandardMaterial { base_color: color, perceptual_roughness: 0.8, ..default() })),
                    ));
                }
            }
        }
        entity.insert(transform);
        keep.insert(id, (entity.id(), look));
    }
    for (_, (entity, _)) in play.pieces.drain() {
        commands.entity(entity).despawn();
    }
    play.pieces = keep;
}

/// Shows what an action did: what the game said, what you got, or why not. Script
/// errors also go to the chat, where the agent sees them next time.
pub(super) fn report(result: Result<Acted, String>, ledger: &Ledger, play: &mut Play, chat: &mut Chat, now_s: f32) {
    match result {
        Ok(acted) if !acted.notes.is_empty() => play.toast(acted.notes.join(" · "), now_s),
        Ok(acted) if !acted.gained.is_empty() => play.toast(describe_gains(ledger, &acted.gained), now_s),
        Ok(_) => {}
        Err(why) => {
            if why.contains("script error") {
                chat.say(Role::System, format!("⚠ {why}"));
                chat.script_errors.push(why.clone());
                let excess = chat.script_errors.len().saturating_sub(3);
                chat.script_errors.drain(..excess);
            }
            play.toast(why, now_s);
        }
    }
}

/// Runs the games' timers (offline; those anchored to pieces only near the hero) and
/// shows what games said on their own.
pub fn run_timers(
    time: Res<Time>,
    mut ledger: ResMut<Ledger>,
    mut play: ResMut<Play>,
    mut chat: ResMut<Chat>,
    heroes: Query<&Transform, With<Player>>,
    world: Res<VoxelWorld>,
    mut last: Local<f32>,
) {
    let now_s = time.elapsed_secs();
    if now_s - *last >= 0.5 {
        *last = now_s;
        let me = ledger.profile.id.clone();
        let players: Vec<(String, [f32; 3])> = heroes.iter().map(|t| (me.clone(), t.translation.to_array())).collect();
        for why in ledger.run_timers(&players, &*world) {
            report(Err(format!("script error: {why}")), &ledger, &mut play, &mut chat, now_s);
        }
    }
    if !ledger.heard.is_empty() {
        let heard = std::mem::take(&mut ledger.heard);
        play.toast(heard.join(" · "), now_s);
    }
}

/// Finds the piece within reach, and uses it on E (unless a game has the keys).
pub fn use_pieces(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    blocked: Res<InputBlocked>,
    creations: Res<Creations>,
    landscape: Res<Landscape>,
    mut ledger: ResMut<Ledger>,
    mut play: ResMut<Play>,
    mut chat: ResMut<Chat>,
    heroes: Query<&Transform, With<Player>>,
) {
    let Ok(hero) = heroes.single() else { return };
    let feet = hero.translation;
    let logic = &ledger.logic;
    play.near = logic
        .state
        .entities()
        .filter(|(_, e)| describe(logic, e).1.is_some())
        .map(|(id, e)| (id, Vec3::from_array(position_at(e, now_exact())).distance(feet)))
        .filter(|(_, d)| *d <= REACH)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(id, _)| id);
    let busy = creations.draft.is_some() || landscape.selecting || landscape.draft.is_some() || play.playing.is_some();
    if blocked.0 || busy {
        return;
    }
    let Some(entity) = play.near else { return };
    let intent = if keys.just_pressed(KeyCode::KeyE) {
        Intent::Interact { entity }
    } else if keys.just_pressed(KeyCode::KeyF) {
        Intent::Hit { entity }
    } else {
        return;
    };
    let result = ledger.act(intent);
    report(result, &ledger, &mut play, &mut chat, time.elapsed_secs());
}

/// Puts the hero where games send them: each teleport once, and on coming into a world,
/// the game's spawn point.
pub fn follow_teleports(ledger: Res<Ledger>, mut seen: Local<(String, u64)>, mut heroes: Query<(&mut Transform, &mut Player)>) {
    let state = &ledger.logic.state;
    let me = &ledger.profile.id;
    let world = super::worlds::current_title(&ledger);
    let mut to = None;
    if seen.0 != world {
        // Just arrived: earlier teleports are history; the game's spawn point isn't.
        seen.0 = world;
        seen.1 = state.teleport(me).map_or(0, |t| t.change);
        to = state.teleport("*").map(|t| t.position);
    } else if let Some(t) = state.teleport(me).filter(|t| t.change > seen.1) {
        seen.1 = t.change;
        to = Some(t.position);
    }
    let Some(to) = to else { return };
    if let Ok((mut transform, mut player)) = heroes.single_mut() {
        transform.translation = Vec3::from_array(to) + Vec3::Y * 0.05;
        player.arrive();
    }
}

/// Takes the hero where games send them out of this world (`travel(player, world)`):
/// each trip once, as through a portal, once any draft is placed or discarded.
pub fn follow_travels(
    ledger: Res<Ledger>,
    mut seen: Local<(String, u64, bool)>,
    mut travel: ResMut<super::worlds::Travel>,
    creations: Res<super::creations::Creations>,
    landscape: Res<super::landscape::Landscape>,
    mut chat: ResMut<super::Chat>,
) {
    let state = &ledger.logic.state;
    let me = &ledger.profile.id;
    let world = super::worlds::current_title(&ledger);
    if seen.0 != world {
        // Just arrived: trips out of here from before are history.
        *seen = (world, state.travel(me).map_or(0, |t| t.change), false);
        return;
    }
    let Some(trip) = state.travel(me).filter(|t| t.change > seen.1) else { return };
    if creations.draft.is_some() || landscape.draft.is_some() {
        if !seen.2 {
            seen.2 = true;
            chat.say(super::Role::System, format!("The game is taking you to {}: place or discard your draft first.", trip.to));
        }
        return;
    }
    (seen.1, seen.2) = (trip.change, false);
    let to = genverse_rules::log::PortalTo::World { name: trip.to.clone() };
    if let Err(why) = travel.portal(&to, &ledger) {
        chat.say(super::Role::System, format!("The game tried to take you to {}. {why}", trip.to));
    }
}

/// How games have the hero move, and each throw once.
pub fn follow_body(ledger: Res<Ledger>, mut seen: Local<(String, u64)>, mut body: ResMut<HeroBody>) {
    let state = &ledger.logic.state;
    let me = &ledger.profile.id;
    let b = state.body(me);
    (body.speed, body.jump, body.gravity, body.air_jumps) = (b.speed, b.jump, b.gravity, b.air_jumps);
    let world = super::worlds::current_title(&ledger);
    let last = state.launch(me).map_or(0, |l| l.change);
    if seen.0 != world {
        // Just arrived: earlier throws are history.
        *seen = (world, last);
    } else if let Some(launch) = state.launch(me).filter(|l| l.change > seen.1) {
        seen.1 = launch.change;
        body.launch = Some(Vec3::from_array(launch.velocity));
    }
}

/// Where the local player's pawn is now (along its walk, if it's walking: the hero rides
/// it), if a game has given them one.
fn my_pawn(ledger: &Ledger) -> Option<Vec3> {
    let me = ledger.profile.id.as_str();
    let (_, pawn) = ledger.logic.state.entities().find(|(_, e)| e.components.get("pawn").and_then(|p| p.as_str()) == Some(me))?;
    Some(Vec3::from_array(position_at(pawn, now_exact())))
}


/// The key's world direction, turned so W is away from the camera.
fn direction(key: KeyCode, yaw: f32) -> Option<&'static str> {
    let turn = match key {
        KeyCode::KeyW | KeyCode::ArrowUp => 0,
        KeyCode::KeyD | KeyCode::ArrowRight => 1,
        KeyCode::KeyS | KeyCode::ArrowDown => 2,
        KeyCode::KeyA | KeyCode::ArrowLeft => 3,
        _ => return None,
    };
    // The camera's forward, snapped to the nearest axis: 0 north (-z), 1 east, ...
    let forward = Vec2::new(-yaw.sin(), -yaw.cos());
    let facing = if forward.x.abs() > forward.y.abs() {
        if forward.x > 0.0 { 1 } else { 3 }
    } else if forward.y < 0.0 {
        0
    } else {
        2
    };
    Some(["north", "east", "south", "west"][(facing + turn) % 4])
}

/// Pawns and regions that take keys: while a game has your pawn in one, your hero
/// stands on it and the movement keys go to the game.
pub fn play_keys(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    blocked: Res<InputBlocked>,
    mut pawned: ResMut<Pawned>,
    mut ledger: ResMut<Ledger>,
    mut play: ResMut<Play>,
    mut chat: ResMut<Chat>,
    cameras: Query<&FollowCamera>,
) {
    let now_s = time.elapsed_secs();
    pawned.0 = my_pawn(&ledger);
    // On their pawn in a region that takes keys, the player's moves go to its game;
    // walking in is the game's to notice (`on_enter`), as for any region.
    play.playing = pawned.0.and_then(|at| keys_region_at(&ledger.logic.state, at.to_array(), now()));
    let Some(region) = play.playing.filter(|_| !blocked.0) else { return };
    let key = |name: &str| Intent::Key { entity: region, key: name.into() };
    let yaw = cameras.single().map_or(0.0, |c| c.yaw());
    let arrows = [KeyCode::KeyW, KeyCode::KeyA, KeyCode::KeyS, KeyCode::KeyD, KeyCode::ArrowUp, KeyCode::ArrowLeft, KeyCode::ArrowDown, KeyCode::ArrowRight];
    let mut send = None;
    if let Some(k) = arrows.into_iter().find(|k| keys.just_pressed(*k)) {
        play.held = Some((k, now_s, now_s));
        send = direction(k, yaw);
    } else if let Some((k, since, last)) = play.held {
        if !keys.pressed(k) {
            play.held = None;
        } else if now_s - since > REPEAT_AFTER && now_s - last > REPEAT_EVERY {
            play.held = Some((k, since, now_s));
            send = direction(k, yaw);
        }
    }
    if keys.just_pressed(KeyCode::KeyR) {
        send = Some("reset");
    } else if keys.just_pressed(KeyCode::KeyE) {
        send = Some("action");
    } else if keys.just_pressed(KeyCode::Escape) {
        send = Some("leave");
    }
    if let Some(name) = send {
        let result = ledger.act(key(name));
        report(result, &ledger, &mut play, &mut chat, now_s);
    }
}

/// "+5 Turnip seeds", "−1 Turnip seeds".
fn describe_gains(ledger: &Ledger, gained: &[(String, i64)]) -> String {
    gained
        .iter()
        .map(|(item, n)| format!("{}{} {}", if *n > 0 { "+" } else { "−" }, n.abs(), item_title(&ledger.logic, item)))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(super) fn item_title(logic: &Logic, item: &str) -> String {
    let package = item.split(':').next().and_then(|ns| package_of(logic, ns));
    // Carried in from a world whose game rules aren't on here: its name will do.
    package.map_or_else(|| item.rsplit(':').next().unwrap_or(item).replace('_', " "), |p| data::title(p, item))
}

/// The toast, and labels over the pieces nearby.
pub fn game_labels(
    inventory: Res<super::inventory::Inventory>,
    mut contexts: EguiContexts,
    time: Res<Time>,
    chat: Res<Chat>,
    ledger: Res<Ledger>,
    mut play: ResMut<Play>,
    cameras: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    heroes: Query<&Transform, With<Player>>,
) -> Result {
    let ctx = contexts.ctx_mut()?;
    let p = theme::palette(ctx);
    let screen = ctx.viewport_rect();
    let covered = chat.covered * screen.width();
    let now_s = time.elapsed_secs();

    if let Some((text, at)) = &play.toast {
        if now_s - at > TOAST_SECONDS {
            play.toast = None;
        } else {
            egui::Area::new("game_toast".into())
                .anchor(egui::Align2::CENTER_TOP, egui::vec2(-covered / 2.0, 24.0))
                .interactable(false)
                .show(ctx, |ui| {
                    egui::Frame::new()
                        .fill(p.overlay)
                        .stroke(egui::Stroke::new(1.0, p.line))
                        .corner_radius(theme::ROUND + 1)
                        .inner_margin(egui::Margin::symmetric(12, 6))
                        .show(ui, |ui| ui.label(egui::RichText::new(text).color(p.strong)));
                });
        }
    }

    let logic = &ledger.logic;
    // Not over the inventory screen.
    if inventory.open {
        return Ok(());
    }
    if let (Ok((camera, eye)), Ok(hero)) = (cameras.single(), heroes.single()) {
        // The one in reach first, then the closest; labels that would cover one already
        // shown are left out.
        let mut nearby: Vec<_> = logic
            .state
            .entities()
            .map(|(id, e)| (play.near != Some(id), Vec3::from_array(position_at(e, now_exact())).distance(hero.translation), id, e))
            .filter(|(_, d, _, _)| *d <= LABEL_DISTANCE)
            .collect();
        nearby.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
        let mut shown: Vec<Vec2> = Vec::new();
        for (_, _, id, e) in nearby {
            let (look, Some(label)) = describe(logic, e) else { continue };
            let top = Vec3::from_array(position_at(e, now_exact())) + Vec3::Y * (play.models.height(logic, &look) + 0.3);
            // Only pieces in front of the camera.
            if (top - eye.translation()).dot(*eye.forward()) <= 0.0 {
                continue;
            }
            let Ok(at) = camera.world_to_viewport(eye, top) else { continue };
            if shown.iter().any(|s| (s.x - at.x).abs() < 160.0 && (s.y - at.y).abs() < 40.0) {
                continue;
            }
            shown.push(at);
            let near = play.near == Some(id) && play.playing.is_none();
            egui::Area::new(egui::Id::new(("game_piece", id.change, id.n)))
                .fixed_pos(egui::pos2(at.x, at.y))
                .pivot(egui::Align2::CENTER_BOTTOM)
                .interactable(false)
                .order(if near { egui::Order::Foreground } else { egui::Order::Middle })
                .show(ctx, |ui| {
                    let frame = egui::Frame::new().corner_radius(6.0).inner_margin(egui::Margin::symmetric(8, 3));
                    if near {
                        frame.fill(p.overlay).show(ui, |ui| {
                            ui.add(egui::Label::new(egui::RichText::new(label).color(p.strong)).extend());
                            ui.add(egui::Label::new(egui::RichText::new("E to use · F to hit").color(p.muted).small()).extend());
                        });
                    } else {
                        frame.fill(p.overlay.gamma_multiply(0.6)).show(ui, |ui| {
                            ui.add(egui::Label::new(egui::RichText::new(label).color(p.text).small()).extend());
                        });
                    }
                });
        }
    }

    Ok(())
}

/// `GENVERSE_PLAY`: see the module docs.
pub fn script_play(
    time: Res<Time>,
    world: Res<VoxelWorld>,
    mut ledger: ResMut<Ledger>,
    mut play: ResMut<Play>,
    mut chat: ResMut<Chat>,
    mut heroes: Query<(&mut Player, &mut Transform)>,
    mut travel: ResMut<super::worlds::Travel>,
    mut inventory: ResMut<super::inventory::Inventory>,
    mut steps: Local<Option<(Vec<String>, f32, usize)>>,
) {
    let now_s = time.elapsed_secs();
    let Ok((mut player, mut hero)) = heroes.single_mut() else { return };
    if chat.pending.is_some() {
        return;
    }
    if steps.is_none() {
        let Ok(script) = std::env::var("GENVERSE_PLAY") else { return };
        if !player.has_landed() {
            return;
        }
        let list = script.split(';').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        *steps = Some((list, now_s + 1.0, 0));
    }
    let Some((list, next, placed)) = steps.as_mut() else { return };
    if list.is_empty() || now_s < *next {
        return;
    }
    *next = now_s + 0.5;
    let mut step = list.remove(0);
    // The inventory: open or close it, or put hotbar slot N (1-9) in hand.
    if step == "inventory" {
        inventory.open = !inventory.open;
        info!("GENVERSE_PLAY inventory: {}", if inventory.open { "open" } else { "closed" });
        return;
    }
    if let Some(n) = step.strip_prefix("slot ").and_then(|n| n.trim().parse::<usize>().ok()) {
        inventory.pick(n.saturating_sub(1), now_s);
        return;
    }
    // Time for the world to happen.
    if let Some(seconds) = step.strip_prefix("wait ").and_then(|s| s.trim().parse::<f32>().ok()) {
        *next = now_s + seconds;
        return;
    }
    // The hero steps to x, z: regions and chasers notice.
    if let Some(to) = step.strip_prefix("go ") {
        let xz: Vec<f32> = to.split_whitespace().filter_map(|v| v.parse().ok()).collect();
        if let [x, z] = xz[..] {
            let down = Ray3d::new(Vec3::new(x, 60.0, z), Dir3::NEG_Y);
            if let Some(hit) = world.raycast(down, 120.0) {
                hero.translation = down.get_point(hit.distance) + Vec3::Y * 0.05;
                player.arrive();
            }
            info!("GENVERSE_PLAY {step}: hero at {}", hero.translation);
        }
        return;
    }
    // Through the portal the hero stands at.
    if step == "portal" {
        let to = travel.near().cloned();
        match to.map(|to| travel.portal(&to, &ledger)) {
            Some(Ok(())) => info!("GENVERSE_PLAY portal: going"),
            Some(Err(why)) => info!("GENVERSE_PLAY portal: {why}"),
            None => info!("GENVERSE_PLAY portal: not at a portal (hero at {}, {} portals)", hero.translation, ledger.portals.len()),
        }
        return;
    }
    if step == "place" {
        step = inventory.held().map_or(step, str::to_string);
    }
    let result = if let Some(choice) = step.strip_prefix("choose ") {
        ledger.act(Intent::Choose { choice: Some(choice.trim().to_string()) })
    } else if step == "close" {
        ledger.act(Intent::Choose { choice: None })
    } else if step == "use" {
        match play.near {
            Some(entity) => ledger.act(Intent::Interact { entity }),
            None => Err("nothing to use".into()),
        }
    } else if step == "hit" {
        match play.near {
            Some(entity) => ledger.act(Intent::Hit { entity }),
            None => Err("nothing in reach to hit".into()),
        }
    } else if genverse_rules::logic::KEYS.contains(&step.as_str()) {
        let at = my_pawn(&ledger).unwrap_or(hero.translation).to_array();
        match keys_region_at(&ledger.logic.state, at, now()) {
            Some(entity) => ledger.act(Intent::Key { entity, key: step.clone() }),
            None => Err("not in a region that takes keys".into()),
        }
    } else {
        // In a row 2 m ahead of the hero.
        let forward = (hero.rotation * Vec3::NEG_Z).with_y(0.0).normalize_or_zero();
        let side = forward.cross(Vec3::Y);
        let spot = hero.translation + forward * 2.0 + side * (*placed as f32 * 0.8 - 0.8);
        *placed += 1;
        let down = Ray3d::new(spot + Vec3::Y * 4.0, Dir3::NEG_Y);
        match world.raycast(down, 12.0) {
            Some(hit) => ledger.act(Intent::Place { item: step.clone(), position: down.get_point(hit.distance).to_array() }),
            None => Err("no ground there".into()),
        }
    };
    match &result {
        Ok(acted) => info!("GENVERSE_PLAY {step}: ok {:?} {:?}", acted.notes, acted.gained),
        Err(why) => info!("GENVERSE_PLAY {step}: {why}"),
    }
    report(result, &ledger, &mut play, &mut chat, now_s);
}
