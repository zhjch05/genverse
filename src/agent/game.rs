//! Playing the games installed in the world (`cubewish_rules::logic`).
//!
//! - **Pieces:** every entity the game rules keep is drawn from its `look`: one of its
//!   package's voxel models, drawn like a build (the simple farming kinds, which have
//!   none, are plain boxes). Pieces with a `label` show it when you're within 10 m; the
//!   one in reach says E uses it, and F attacks it (`on_hit`).
//! - The hero goes where games send them: a teleport, and on coming into a world, the
//!   game's spawn point.
//! - **The game bar** above the hints lists what you can put down: the fixtures (a game
//!   board) and sources the installed packages define, and the seeds you carry. Pick one,
//!   then click the ground (Esc or right-click puts it away). It also shows the day.
//! - **Zones:** walking into an entity's `zone` tells its game ("enter"). When the game
//!   gives you a `pawn`, your hero stands on it and the movement keys go to the game as
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
//! Script it: `CUBEWISH_PLAY="sokoban:board; enter; east; east"` puts things down in
//! front of the hero (`place` is the game bar's first fixture), uses the nearest piece
//! (`use`), goes through the portal the hero stands at (`portal`) or sends a key to the
//! zone the hero is in, one step every half second once
//! the hero has landed and the agent is done with `CUBEWISH_PROMPT`.

use std::collections::HashMap;

use bevy::{prelude::*, window::PrimaryWindow};
use bevy_egui::{EguiContexts, egui};
use std::f32::consts::FRAC_PI_2;

use cubewish_rules::{
    build::{BuildLimits, compile},
    logic::{
        Entity as Piece, EntityId, Intent, Logic, Package,
        data::{self, Def},
        trial::{self, pawn_of},
        walk::{position_at, walking_at},
    },
};

use super::{Chat, Role, creations::Creations, cursor_ray, landscape::Landscape, theme};
use crate::{
    player::{FollowCamera, InputBlocked, Pawned, Player, PointerOverUi},
    voxel::{DepthBias, ModelData, VoxelModel},
    world::{
        VoxelWorld,
        ledger::{Acted, Ledger},
    },
};

/// How close the hero must be to use a piece, in meters.
const REACH: f32 = 2.5;
/// Pieces this close get a name label, so they're easy to find.
const LABEL_DISTANCE: f32 = 10.0;
/// How far a placed item may be from the hero.
const PLACE_REACH: f32 = 8.0;
const TOAST_SECONDS: f32 = 3.0;
/// A held direction key repeats after this long, this often.
const REPEAT_AFTER: f32 = 0.35;
const REPEAT_EVERY: f32 = 0.16;

#[derive(Resource, Default)]
pub struct Play {
    /// The item or fixture being put down: the next click in the world places it.
    pub holding: Option<String>,
    /// The piece E would use: the nearest labeled one within reach.
    pub near: Option<EntityId>,
    /// The zone whose game has the local player's pawn: it gets the movement keys.
    pub playing: Option<EntityId>,
    /// The zone the hero stood in last frame, to notice walking in.
    inside: Option<EntityId>,
    /// The direction key held, since when, and when it last repeated.
    held: Option<(KeyCode, f32, f32)>,
    /// A short note at the top of the view, and when it appeared.
    toast: Option<(String, f32)>,
    /// The pieces in the world and how each looks.
    pieces: HashMap<EntityId, (Entity, Look)>,
    models: Models,
}

impl Play {
    pub fn toast(&mut self, text: impl Into<String>, now: f32) {
        self.toast = Some((text.into(), now));
    }

    /// Forgets what was going on in the world left behind: holding, playing, near.
    pub fn leave_world(&mut self) {
        self.holding = None;
        self.near = None;
        self.playing = None;
        self.inside = None;
        self.held = None;
    }
}

/// How a piece is drawn.
#[derive(Clone, PartialEq)]
enum Look {
    /// One of its package's voxel models (by package hash and name), turned by quarter
    /// turns.
    Model { package: String, model: String, turn: u8 },
    Crop { stage: usize, stages: usize, ripe: bool },
    Source,
    /// Nothing to draw (a pawn: the hero shows it).
    Hidden,
    /// Its package isn't installed (any more): kept, but asleep.
    Dormant,
}

impl Look {
    /// For the simple farming kinds, which have no models: a box's size and color.
    fn block(&self) -> Option<(Vec3, Color)> {
        Some(match self {
            Look::Crop { ripe: true, .. } => (Vec3::new(0.4, 0.45, 0.4), Color::srgb(0.95, 0.72, 0.25)),
            Look::Crop { stage, stages, .. } => {
                let grown = (*stage as f32 + 1.0) / (*stages as f32 + 1.0);
                (Vec3::new(0.15 + 0.2 * grown, 0.1 + 0.3 * grown, 0.15 + 0.2 * grown), Color::srgb(0.35, 0.7, 0.3))
            }
            Look::Source => (Vec3::splat(0.6), Color::srgb(0.55, 0.38, 0.22)),
            Look::Dormant => (Vec3::splat(0.2), Color::srgb(0.5, 0.5, 0.5)),
            Look::Model { .. } | Look::Hidden => return None,
        })
    }
}

/// Compiled voxel models, by package hash and model name (none if it doesn't compile).
#[derive(Default)]
struct Models(HashMap<(String, String), Option<ModelData>>);

impl Models {
    fn get(&mut self, logic: &Logic, package: &str, model: &str) -> Option<&ModelData> {
        self.0
            .entry((package.to_string(), model.to_string()))
            .or_insert_with(|| {
                let spec = logic.packages.get(package)?.models.get(model)?;
                compile(spec, &BuildLimits::ACCEPTED).map(|b| b.model).ok()
            })
            .as_ref()
    }

    /// How tall a piece stands, in meters.
    fn height(&mut self, logic: &Logic, look: &Look) -> f32 {
        match look {
            Look::Model { package, model, .. } => self.get(logic, package, model).map_or(0.0, |m| m.grid.size().y as f32 * m.voxel_size),
            other => other.block().map_or(0.0, |(size, _)| size.y),
        }
    }
}

/// Package `package`, if it's installed.
fn package_of<'a>(logic: &'a Logic, package: &str) -> Option<&'a Package> {
    logic.installed.get(package).and_then(|i| logic.package(i))
}

/// How a piece looks and what it's called, from its components and today's date.
fn describe(logic: &Logic, e: &Piece, today: u64) -> (Look, Option<String>) {
    let label = e.components.get("label").and_then(|l| l.as_str()).map(str::to_string);
    let Some(package) = package_of(logic, &e.package) else {
        return (Look::Dormant, Some(format!("{} (not installed)", e.package)));
    };
    if let Some(look) = e.components.get("look") {
        let installed = logic.installed.get(&e.package).map(|i| i.package.hash.clone()).unwrap_or_default();
        let model = look["model"].as_str().unwrap_or_default().to_string();
        let turn = look["turn"].as_u64().unwrap_or(0).min(3) as u8;
        return (Look::Model { package: installed, model, turn }, label);
    }
    // A shop or a spawner from data alone is drawn like a seed bin.
    if e.components.contains_key("shop") || e.components.contains_key("spawner") {
        return (Look::Source, label);
    }
    let defs = data::defs(package);
    for (component, value) in &e.components {
        match defs.get(component.as_str()) {
            Some(Def::Crop { title, stage_days, .. }) => {
                let planted = value["planted"].as_u64().unwrap_or(today);
                let (stage, ripe) = data::crop_stage(stage_days, planted, today);
                let text = if ripe { format!("{title} · ripe") } else { format!("{title} · growing {}/{}", stage + 1, stage_days.len()) };
                return (Look::Crop { stage, stages: stage_days.len(), ripe }, Some(label.unwrap_or(text)));
            }
            Some(Def::Source { title, .. }) => return (Look::Source, Some(label.unwrap_or(title.clone()))),
            _ => {}
        }
    }
    (Look::Hidden, label)
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Now, to the millisecond: where walking pieces are.
fn now_exact() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// Where a piece stands now and how it's turned: along its walk, facing the way it
/// goes, if it's walking.
fn placement(e: &Piece, turn: u8, look: &Look) -> Transform {
    let (at, yaw) = walking_at(e, now_exact()).unwrap_or((e.position, turn as f32 * FRAC_PI_2));
    let mut transform = Transform::from_translation(Vec3::from_array(at)).with_rotation(Quat::from_rotation_y(yaw));
    if let Some((size, _)) = look.block() {
        transform.translation.y += size.y / 2.0;
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
    let today = ledger.rules.logic.clock.at(now()).day;
    let play = &mut *play;
    let mut keep = HashMap::new();
    for (id, e) in logic.state.entities() {
        let (look, _) = describe(logic, e, today);
        let turn = if let Look::Model { turn, .. } = look { turn } else { 0 };
        let mut transform = placement(e, turn, &Look::Hidden);
        if let Some((entity, old)) = play.pieces.remove(&id) {
            if old == look {
                let transform = placement(e, turn, &look);
                commands.entity(entity).insert(transform);
                keep.insert(id, (entity, look));
                continue;
            }
            commands.entity(entity).despawn();
        }
        let mut entity = commands.spawn((Visibility::default(), Name::new("Game piece")));
        match &look {
            Look::Model { package, model, .. } => {
                if let Some(data) = play.models.get(logic, package, model) {
                    // Pieces stand on the ground, so their faces often meet it, as builds' do.
                    entity.insert((VoxelModel::from(data), DepthBias(500.0)));
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
    let today = ledger.rules.logic.clock.at(now()).day;
    let logic = &ledger.logic;
    play.near = logic
        .state
        .entities()
        .filter(|(_, e)| describe(logic, e, today).1.is_some())
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

/// Where the local player's pawn stands, if a game has given them one.
fn my_pawn(ledger: &Ledger) -> Option<Vec3> {
    pawn_of(&ledger.logic.state, &ledger.profile.id).map(Vec3::from_array)
}

/// The zone at `at`, if any.
fn zone_at(logic: &Logic, at: Vec3) -> Option<EntityId> {
    trial::zone_at(&logic.state, at.to_array())
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

/// Zones and pawns: walking into a zone tells its game; while the game has your pawn,
/// your hero stands on it and the movement keys go to the game.
pub fn play_zones(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    blocked: Res<InputBlocked>,
    mut pawned: ResMut<Pawned>,
    mut ledger: ResMut<Ledger>,
    mut play: ResMut<Play>,
    mut chat: ResMut<Chat>,
    heroes: Query<&Transform, With<Player>>,
    cameras: Query<&FollowCamera>,
) {
    let Ok(hero) = heroes.single() else { return };
    let now_s = time.elapsed_secs();
    pawned.0 = my_pawn(&ledger);
    let inside = zone_at(&ledger.logic, pawned.0.unwrap_or(hero.translation));
    play.playing = inside.filter(|_| pawned.0.is_some());
    let entered = inside.is_some() && inside != play.inside;
    play.inside = inside;
    let Some(zone) = inside.filter(|_| !blocked.0) else { return };
    let key = |name: &str| Intent::Key { entity: zone, key: name.into() };
    if entered && pawned.0.is_none() {
        let result = ledger.act(key("enter"));
        report(result, &ledger, &mut play, &mut chat, now_s);
        return;
    }
    if play.playing.is_none() {
        return;
    }
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

fn item_title(logic: &Logic, item: &str) -> String {
    let package = item.split(':').next().and_then(|ns| package_of(logic, ns));
    // Carried in from a world whose game rules aren't on here: its name will do.
    package.map_or_else(|| item.rsplit(':').next().unwrap_or(item).replace('_', " "), |p| data::title(p, item))
}

/// While holding something, a click on the ground puts it there; right-click or Esc
/// puts it away.
pub fn place_held(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
    blocked: Res<InputBlocked>,
    windows: Query<&Window, With<PrimaryWindow>>,
    cameras: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    over_ui: Res<PointerOverUi>,
    world: Res<VoxelWorld>,
    heroes: Query<&Transform, With<Player>>,
    mut ledger: ResMut<Ledger>,
    mut play: ResMut<Play>,
    mut chat: ResMut<Chat>,
    mut pressed_at: Local<Option<Vec2>>,
) {
    let Some(item) = play.holding.clone() else {
        *pressed_at = None;
        return;
    };
    if (!blocked.0 && keys.just_pressed(KeyCode::Escape)) || buttons.just_pressed(MouseButton::Right) {
        play.holding = None;
        return;
    }
    let Some((cursor, ray)) = cursor_ray(&windows, &cameras, &over_ui) else { return };
    if buttons.just_pressed(MouseButton::Left) {
        *pressed_at = Some(cursor);
    }
    // A click, not a drag to look around.
    if !buttons.just_released(MouseButton::Left) || !pressed_at.take().is_some_and(|at| at.distance(cursor) <= 5.0) {
        return;
    }
    let now_s = time.elapsed_secs();
    let Some(hit) = world.raycast(ray, 64.0) else { return };
    if hit.normal.y < 0.5 {
        play.toast("Put it on top of the ground.", now_s);
        return;
    }
    let at = ray.get_point(hit.distance);
    if heroes.single().is_ok_and(|h| h.translation.distance(at) > PLACE_REACH) {
        play.toast("That's too far away; walk closer.", now_s);
        return;
    }
    let had = ledger.logic.state.items(&ledger.profile.id, &item) as i64;
    let result = ledger.act(Intent::Place { item: item.clone(), position: at.to_array() });
    if let Ok(acted) = &result {
        // Fixtures go down once; seeds until there are none left.
        let used_up = acted.gained.iter().any(|(i, n)| *i == item && had + n <= 0);
        if used_up || !acted.gained.iter().any(|(i, _)| *i == item) {
            play.holding = None;
        }
        if acted.notes.is_empty() {
            play.toast(format!("Put down {}.", item_title(&ledger.logic, &item)), now_s);
            return;
        }
    }
    report(result, &ledger, &mut play, &mut chat, now_s);
}

/// What the local player can put down: every installed fixture and source, and the
/// items they carry that plant something. Title, name, and how many (none for those
/// that don't run out).
fn placeable(ledger: &Ledger) -> Vec<(String, String, Option<u32>)> {
    let logic = &ledger.logic;
    let mut out = Vec::new();
    let mut seeds = Vec::new();
    for install in logic.installed.values() {
        let Some(package) = logic.package(install) else { continue };
        for (name, def) in data::defs(package) {
            match def {
                Def::Fixture { title }
                | Def::Source { title, .. }
                | Def::Shop { title, .. }
                | Def::Spawner { title, .. }
                | Def::Npc { title, .. } => out.push((title, name.to_string(), None)),
                Def::Crop { seed, .. } => seeds.push(seed),
                Def::Item { .. } => {}
            }
        }
    }
    for (item, n) in logic.state.inventory(&ledger.profile.id) {
        if seeds.iter().any(|s| s == item) {
            out.push((item_title(logic, item), item.to_string(), Some(n)));
        }
    }
    out
}

/// The game bar, the toast, and labels over the pieces nearby.
pub fn game_bar(
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
    let today = ledger.rules.logic.clock.at(now());
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
            let (look, Some(label)) = describe(logic, e, today.day) else { continue };
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

    if logic.installed.is_empty() && logic.state.inventory(&ledger.profile.id).next().is_none() {
        return Ok(());
    }
    let spots = placeable(&ledger);
    let carried: Vec<(String, u32)> = logic
        .state
        .inventory(&ledger.profile.id)
        .filter(|(item, _)| !spots.iter().any(|(_, name, _)| name == item))
        .map(|(item, n)| (item_title(logic, item), n))
        .collect();
    let mut pick = None;
    egui::Area::new("game_bar".into())
        .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(-covered / 2.0, -96.0))
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(p.overlay)
                .stroke(egui::Stroke::new(1.0, p.line))
                .corner_radius(theme::ROUND + 1)
                .inner_margin(egui::Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let season = cubewish_rules::logic::clock::SEASONS[today.season];
                        ui.label(egui::RichText::new(format!("Day {} · {season}", today.day_of_season + 1)).color(p.muted));
                        for (title, name, count) in &spots {
                            ui.separator();
                            let text = match count {
                                Some(n) => format!("{title} ×{n}"),
                                None => title.clone(),
                            };
                            let held = play.holding.as_deref() == Some(name.as_str());
                            if ui.selectable_label(held, text).on_hover_text("Pick it, then click the ground").clicked() {
                                pick = Some(if held { None } else { Some(name.clone()) });
                            }
                        }
                        for (title, n) in &carried {
                            ui.label(egui::RichText::new(format!("{title} ×{n}")).color(p.text));
                        }
                    });
                });
        });
    if let Some(pick) = pick {
        play.holding = pick;
    }
    Ok(())
}

/// `CUBEWISH_PLAY`: see the module docs.
pub fn script_play(
    time: Res<Time>,
    world: Res<VoxelWorld>,
    mut ledger: ResMut<Ledger>,
    mut play: ResMut<Play>,
    mut chat: ResMut<Chat>,
    mut heroes: Query<(&mut Player, &mut Transform)>,
    mut travel: ResMut<super::worlds::Travel>,
    mut steps: Local<Option<(Vec<String>, f32, usize)>>,
) {
    let now_s = time.elapsed_secs();
    let Ok((mut player, mut hero)) = heroes.single_mut() else { return };
    if chat.pending.is_some() {
        return;
    }
    if steps.is_none() {
        let Ok(script) = std::env::var("CUBEWISH_PLAY") else { return };
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
            info!("CUBEWISH_PLAY {step}: hero at {}", hero.translation);
        }
        return;
    }
    // Through the portal the hero stands at.
    if step == "portal" {
        let to = travel.near().cloned();
        match to.map(|to| travel.portal(&to, &ledger)) {
            Some(Ok(())) => info!("CUBEWISH_PLAY portal: going"),
            Some(Err(why)) => info!("CUBEWISH_PLAY portal: {why}"),
            None => info!("CUBEWISH_PLAY portal: not at a portal (hero at {}, {} portals)", hero.translation, ledger.portals.len()),
        }
        return;
    }
    if step == "place" {
        let fixture = placeable(&ledger).into_iter().find(|(_, name, count)| count.is_none() && name.contains(':'));
        step = fixture.map_or(step, |(_, name, _)| name);
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
    } else if cubewish_rules::logic::KEYS.contains(&step.as_str()) {
        match zone_at(&ledger.logic, my_pawn(&ledger).unwrap_or(hero.translation)) {
            Some(entity) => ledger.act(Intent::Key { entity, key: step.clone() }),
            None => Err("not in a game's zone".into()),
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
        Ok(acted) => info!("CUBEWISH_PLAY {step}: ok {:?} {:?}", acted.notes, acted.gained),
        Err(why) => info!("CUBEWISH_PLAY {step}: {why}"),
    }
    report(result, &ledger, &mut play, &mut chat, now_s);
}
