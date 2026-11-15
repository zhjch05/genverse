//! The hero: a voxel character assembled from model parts, a kinematic controller with
//! voxel collision, procedural walk animation, and a third-person follow camera that
//! eases in front of whatever would hide most of the hero (lesser things in the way
//! are dithered away instead; see [`CameraCutout`]).

use std::f32::consts::{PI, TAU};

use bevy::{
    input::mouse::{AccumulatedMouseMotion, AccumulatedMouseScroll, MouseScrollUnit},
    math::Affine3A,
    platform::collections::HashMap,
    prelude::*,
    window::{CursorOptions, PrimaryWindow},
};

use genverse_rules::logic::solid::Areas;

use crate::bindings::Bindings;
use crate::{
    voxel::{
        CameraCutout, VoxelModel, VoxelModelAsset,
        query::{overlaps_box, solid_at},
    },
    world::{
        ChunkLoader, SolidModel, VoxelWorld,
        terrain::{Terrain, VOXEL_SIZE, is_solid},
    },
};

const HALF_WIDTH: f32 = 0.28;
const HEIGHT: f32 = 1.35;
pub const WALK_SPEED: f32 = 4.0;
pub const RUN_SPEED: f32 = 7.0;
const JUMP_SPEED: f32 = 7.5;
pub const GRAVITY: f32 = 24.0;
/// Highest ledge the hero walks up without jumping.
const STEP_HEIGHT: f32 = 2.0 * VOXEL_SIZE;

pub struct PlayerPlugin;

impl Plugin for PlayerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<InputBlocked>()
            .init_resource::<Pawned>()
            .init_resource::<HeroBody>()
            .init_resource::<ScriptedHero>()
            .init_resource::<PointerOverUi>()
            .init_resource::<OrbitLock>()
            .init_resource::<Showcase>()
            .init_resource::<SolidAreas>()
            .add_systems(Startup, spawn_player)
            .add_systems(
                Update,
                (
                    build_hero_parts,
                    index_solids,
                    move_player,
                    animate_hero,
                    follow_camera,
                )
                    .chain(),
            );
    }
}

/// Set while a UI (like the builder chat) is taking keyboard input.
#[derive(Resource, Default)]
pub struct InputBlocked(pub bool);

/// Where a game holds the hero, while it does: the hero walks there by itself and the
/// movement keys belong to the game (see `agent::game`).
#[derive(Resource, Default)]
pub struct Pawned(pub Option<Vec3>);

/// How a game has the hero move, each a multiple of the usual (see `agent::game`), and a
/// throw it hasn't felt yet.
#[derive(Resource)]
pub struct HeroBody {
    /// How heroes move in this world, before a game's multiples: meters a second
    /// walking and running, how fast a jump leaves the ground, and gravity.
    pub walk: f32,
    pub run: f32,
    pub jump_speed: f32,
    pub fall: f32,
    pub speed: f32,
    /// How high a jump goes.
    pub jump: f32,
    pub gravity: f32,
    pub air_jumps: u8,
    pub launch: Option<Vec3>,
}

impl Default for HeroBody {
    fn default() -> Self {
        Self {
            walk: WALK_SPEED,
            run: RUN_SPEED,
            jump_speed: JUMP_SPEED,
            fall: GRAVITY,
            speed: 1.0,
            jump: 1.0,
            gravity: 1.0,
            air_jumps: 0,
            launch: None,
        }
    }
}

/// What a game's player scripts decided this frame (see `agent::player_scripts`): the
/// hero's velocity in place of the usual controls, and how the camera follows.
#[derive(Resource, Default)]
pub struct ScriptedHero {
    pub velocity: Option<Vec3>,
    /// A script asked for the usual jump this frame (`jump()`).
    pub jump: bool,
    pub camera: Option<CameraRig>,
}

/// How a script has the camera follow the hero; unset parts stay the player's.
#[derive(Clone, Copy, Debug, Default)]
pub struct CameraRig {
    /// Meters behind the hero; 0 looks out of their eyes.
    pub distance: Option<f32>,
    pub pitch: Option<f32>,
    pub yaw: Option<f32>,
    /// Meters the camera's target rises above the hero's head.
    pub height: Option<f32>,
    pub shake: f32,
}

/// Set while the pointer is over a UI, whose scrolling isn't the camera's zoom.
#[derive(Resource, Default)]
pub struct PointerOverUi(pub bool);

/// While set, left-drag no longer turns the camera (another tool owns it); right-drag still does.
#[derive(Resource, Default)]
pub struct OrbitLock(pub bool);

/// While set, the camera circles the hero slowly from a little way off, as a backdrop
/// (the start screen).
#[derive(Resource, Default)]
pub struct Showcase(pub bool);

#[derive(Component)]
pub struct Player {
    velocity: Vec3,
    grounded: bool,
    /// Yaw the model faces, radians.
    facing: f32,
    /// Walk cycle phase, radians.
    stride: f32,
    /// Smoothed horizontal speed, drives the animation amplitude.
    pace: f32,
    /// How far the model sits above (+) or below (-) the body after a step snaps it, eased to zero.
    lag: f32,
    spawned: bool,
    /// Jumps made since leaving the ground.
    air_jumps: u8,
}

#[derive(Component)]
pub struct Hero {
    model: Handle<VoxelModelAsset>,
    parts: HashMap<String, Entity>,
}

impl Player {
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))] // Player scripts are desktop-only for now.
    pub fn velocity(&self) -> Vec3 {
        self.velocity
    }

    #[cfg_attr(target_arch = "wasm32", allow(dead_code))] // Player scripts are desktop-only for now.
    pub fn grounded(&self) -> bool {
        self.grounded
    }

    /// Radians around Y, 0 facing north (-z).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))] // Player scripts are desktop-only for now.
    pub fn facing(&self) -> f32 {
        self.facing
    }

    /// True once the ground under the spawn point has loaded and the hero stands on it.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))] // Used by the desktop-only builder.
    pub fn has_landed(&self) -> bool {
        self.spawned && self.grounded
    }

    /// Starts over at a new spot (another world): it waits for the ground there.
    pub fn arrive(&mut self) {
        self.velocity = Vec3::ZERO;
        self.grounded = false;
        self.spawned = false;
        self.lag = 0.0;
    }
}

/// Where the hero starts on `terrain`: the first dry ground walking out from `origin`
/// (in voxels), feet in meters. On the varied land of version 3 and up, the first such
/// ground with a view: open around, and water in sight.
pub fn spawn_point(terrain: Terrain, origin: IVec2) -> Vec3 {
    let scenic = !terrain.flat && terrain.version >= 3;
    let (mut x, mut z) = (origin.x, origin.y);
    let mut dry = None;
    // A spiral out to about 300 m: past the widest lakes and bays.
    for i in 0..4000 {
        let angle = i as f32 * 2.4;
        let r = (i as f32).sqrt() * 20.0;
        (x, z) = (
            origin.x + (angle.cos() * r) as i32,
            origin.y + (angle.sin() * r) as i32,
        );
        if terrain.height(x, z) <= terrain.sea_level() + 2 {
            continue;
        }
        dry.get_or_insert((x, z));
        // Worlds from before keep the start their builds grew around.
        if !scenic || has_a_view(terrain, x, z) {
            break;
        }
        if i >= 1500 {
            // Nowhere near has one: the first dry ground it is.
            (x, z) = dry.unwrap_or((x, z));
            break;
        }
    }
    let ground = terrain.height(x, z) + 1;
    Vec3::new(x as f32 + 0.5, ground as f32 + 1.0, z as f32 + 0.5) * VOXEL_SIZE
}

/// Whether the ground around column `(x, z)` is open enough to look around from (no
/// hill or cliff close by, which the camera would have to duck in front of) and has
/// water within sight.
fn has_a_view(terrain: Terrain, x: i32, z: i32) -> bool {
    let here = terrain.height(x, z);
    let around = |r: f32| {
        (0..12)
            .map(move |i| i as f32 * std::f32::consts::TAU / 12.0)
            .map(move |a| (x + (a.cos() * r) as i32, z + (a.sin() * r) as i32))
    };
    // Voxels away, and how far the ground may rise by there.
    let open = [(8.0, 3), (16.0, 5), (40.0, 8), (72.0, 14)]
        .into_iter()
        .all(|(r, rise)| {
            around(r).all(|(x, z)| (here - 24..=here + rise).contains(&terrain.height(x, z)))
        });
    open && [70.0, 100.0, 130.0, 160.0]
        .into_iter()
        .any(|r| around(r).any(|(x, z)| terrain.height(x, z) < terrain.sea_level()))
}

/// One animated limb; `rest` is its translation relative to its parent joint.
#[derive(Component)]
struct Limb {
    name: String,
    rest: Vec3,
}

/// Where the camera pivots: the hero's eyes, meters above their feet.
const EYE: f32 = 1.2;
/// How far in front of the eyes the camera sits when a game holds it there (a
/// negative reach).
const FIRST_PERSON: f32 = -0.15;
/// Half the width of the camera's probe for things in the way: wide enough that the
/// corners of the near plane stay out of walls too, not just its center.
const PROBE: f32 = 0.22;
/// Room kept between the camera and whatever is in the way.
const MARGIN: f32 = 0.2;
/// How far down the start screen's circling camera looks over open ground, and the
/// most it looks down to clear a hill, radians.
const SHOWCASE_PITCH: f32 = 0.3;
const SHOWCASE_STEEPEST: f32 = 1.25;
/// How far below the eyes the middle of the hero is, where the cutout centers.
const CUTOUT_DROP: f32 = 0.5;
/// The cutout's radius around the hero, in meters: the whole hero and a little more.
const CUTOUT_RADIUS: f32 = 1.3;

#[derive(Component)]
pub struct FollowCamera {
    yaw: f32,
    pitch: f32,
    /// Meters behind the hero the player asked for (scroll).
    distance: f32,
    /// `distance` eased, so zooming glides.
    zoom: f32,
    /// How far something in the way pulls the camera in from `zoom`, eased: in quickly,
    /// back out slowly, so it never jumps.
    pull: f32,
    /// A drag holds the cursor, so it doesn't run off the window (desktop only).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    grabbed: bool,
    target: Vec3,
    /// How far down the start screen's circling camera wants to look: higher while a
    /// hill is in its way, so it rises over the hill rather than ducking in front of it.
    crane: f32,
    /// The pitch and distance the player had before the start screen's camera took
    /// over, to go back to after.
    before: Option<(f32, f32)>,
    /// The pitch to ease back to after the start screen, until the player looks around.
    settle: Option<f32>,
}

impl FollowCamera {
    /// Which way the camera looks, radians around Y (0 looks north, -z).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))] // Player scripts are desktop-only for now.
    pub fn yaw(&self) -> f32 {
        self.yaw
    }

    /// How far it looks down, radians.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))] // Player scripts are desktop-only for now.
    pub fn pitch(&self) -> f32 {
        self.pitch
    }
}

impl Default for FollowCamera {
    fn default() -> Self {
        let env = |k: &str, d: f32| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        let distance = env("GENVERSE_DIST", 4.5);
        Self {
            yaw: env("GENVERSE_YAW", 0.6),
            pitch: env("GENVERSE_PITCH", 0.35),
            distance,
            zoom: distance,
            pull: 0.0,
            grabbed: false,
            target: Vec3::ZERO,
            crane: SHOWCASE_PITCH,
            before: None,
            settle: None,
        }
    }
}

fn spawn_player(mut commands: Commands, asset_server: Res<AssetServer>, world: Res<VoxelWorld>) {
    // Walk outward from the origin to the first dry column. `GENVERSE_SPAWN=x,z` (meters)
    // starts elsewhere, e.g. to keep test players apart.
    let terrain = world.terrain;
    let origin = std::env::var("GENVERSE_SPAWN")
        .ok()
        .and_then(|v| {
            v.split_once(',').and_then(|(x, z)| {
                Some((x.trim().parse::<f32>().ok()?, z.trim().parse::<f32>().ok()?))
            })
        })
        .map_or(IVec2::ZERO, |(x, z)| {
            (Vec2::new(x, z) / VOXEL_SIZE).as_ivec2()
        });
    let start = spawn_point(terrain, origin);

    commands
        .spawn((
            Name::new("Hero"),
            Player {
                velocity: Vec3::ZERO,
                grounded: false,
                facing: PI,
                stride: 0.0,
                pace: 0.0,
                lag: 0.0,
                spawned: false,
                air_jumps: 0,
            },
            ChunkLoader,
            Transform::from_translation(start),
            Visibility::default(),
        ))
        .with_child((
            Hero {
                model: asset_server.load("characters/hero.vox.ron"),
                parts: HashMap::new(),
            },
            Transform::default(),
            Visibility::default(),
        ));
}

/// (Re)builds the hero's limb entities whenever the model loads or changes on disk.
fn build_hero_parts(
    mut commands: Commands,
    mut events: MessageReader<AssetEvent<VoxelModelAsset>>,
    assets: Res<Assets<VoxelModelAsset>>,
    mut heroes: Query<(Entity, &mut Hero)>,
) {
    let changed: Vec<_> = events
        .read()
        .filter_map(|e| match e {
            AssetEvent::LoadedWithDependencies { id } | AssetEvent::Modified { id } => Some(*id),
            _ => None,
        })
        .collect();
    for (root, mut hero) in &mut heroes {
        if !changed.contains(&hero.model.id()) {
            continue;
        }
        let Some(asset) = assets.get(&hero.model) else {
            continue;
        };
        hero.parts.clear();
        commands.entity(root).despawn_related::<Children>();
        // Parents must exist before children; the file lists them in any order.
        let mut pending: Vec<_> = asset.parts.iter().collect();
        while !pending.is_empty() {
            let before = pending.len();
            pending.retain(|part| {
                let (parent_entity, parent_joint) = match &part.parent {
                    None => (root, asset.pivot),
                    Some(name) => match (
                        hero.parts.get(name),
                        asset.parts.iter().find(|p| &p.name == name),
                    ) {
                        (Some(e), Some(p)) => (*e, p.joint),
                        _ => return true,
                    },
                };
                let rest = (part.joint - parent_joint) * asset.voxel_size;
                let entity = commands
                    .spawn((
                        Name::new(part.name.clone()),
                        VoxelModel::from_part(asset, part),
                        Limb {
                            name: part.name.clone(),
                            rest,
                        },
                        Transform::from_translation(rest),
                        ChildOf(parent_entity),
                    ))
                    .id();
                hero.parts.insert(part.name.clone(), entity);
                false
            });
            if pending.len() == before {
                warn!(
                    "hero parts have unknown or cyclic parents: {:?}",
                    pending.iter().map(|p| &p.name).collect::<Vec<_>>()
                );
                break;
            }
        }
    }
}

/// The hero's collision box for feet at `feet`, as `(min, max)`.
pub fn bounds(feet: Vec3) -> (Vec3, Vec3) {
    (
        feet - Vec3::new(HALF_WIDTH, 0.0, HALF_WIDTH),
        feet + Vec3::new(HALF_WIDTH, HEIGHT, HALF_WIDTH),
    )
}

/// Everything the hero can bump into: terrain plus nearby solid models.
struct Obstacles<'a> {
    world: &'a VoxelWorld,
    models: Vec<(&'a VoxelModel, &'a GlobalTransform)>,
}

impl Obstacles<'_> {
    fn collides(&self, feet: Vec3) -> bool {
        let (lo, hi) = bounds(feet);
        let min = VoxelWorld::voxel_at(lo);
        let max = VoxelWorld::voxel_at(hi - 1e-4);
        for z in min.z..=max.z {
            for y in min.y..=max.y {
                for x in min.x..=max.x {
                    // Unloaded space counts as solid so the hero never falls through it.
                    if self.world.get(IVec3::new(x, y, z)).is_none_or(is_solid) {
                        return true;
                    }
                }
            }
        }
        self.models
            .iter()
            .any(|(model, transform)| overlaps_box(model, transform, lo, hi))
    }
}

/// Solid models by the ground they reach, so the hero only looks at the ones near it.
#[derive(Resource, Default)]
pub struct SolidAreas(Areas<Entity>);

/// How far from where a model stands it might touch the hero this frame.
fn reach(model: &VoxelModel) -> f32 {
    model.grid.size().as_vec3().length() * model.voxel_size + 2.0
}

fn index_solids(
    mut areas: ResMut<SolidAreas>,
    moved: Query<
        (Entity, &VoxelModel, &GlobalTransform),
        (
            With<SolidModel>,
            Or<(
                Added<SolidModel>,
                Changed<GlobalTransform>,
                Changed<VoxelModel>,
            )>,
        ),
    >,
    mut gone: RemovedComponents<SolidModel>,
) {
    for entity in gone.read() {
        areas.0.take(entity);
    }
    for (entity, model, transform) in &moved {
        areas
            .0
            .put(entity, transform.translation().to_array(), reach(model));
    }
}

pub fn move_player(
    time: Res<Time>,
    binds: Res<Bindings>,
    blocked: Res<InputBlocked>,
    pawned: Res<Pawned>,
    mut body: ResMut<HeroBody>,
    scripted: Res<ScriptedHero>,
    world: Res<VoxelWorld>,
    cameras: Query<&FollowCamera>,
    solids: Query<(&VoxelModel, &GlobalTransform), With<SolidModel>>,
    areas: Res<SolidAreas>,
    mut players: Query<(&mut Player, &mut Transform)>,
) {
    let dt = time.delta_secs().min(1.0 / 20.0);
    let yaw = cameras.single().map_or(0.0, |c| c.yaw);
    for (mut player, mut transform) in &mut players {
        if !world.column_loaded(transform.translation) {
            continue;
        }
        // Only models whose bounds could reach the hero this frame.
        let here = transform.translation;
        let obstacles = Obstacles {
            world: &world,
            models: areas
                .0
                .within([here.x, here.z], [here.x, here.z])
                .into_iter()
                .filter_map(|entity| solids.get(entity).ok())
                .filter(|(model, t)| t.translation().distance(here) < reach(model))
                .collect(),
        };
        // Inside something solid (a build or a game's gate appeared there, the hero
        // arrived in one, or got off a ride standing in its floor): step out to the
        // nearest free spot: up onto a floor, else sideways, else up onto it; buried
        // deeper (spawning under ground), straight up.
        if pawned.0.is_none() && obstacles.collides(transform.translation) {
            let here = transform.translation;
            let up = |h: i32| Vec3::Y * (h as f32 * VOXEL_SIZE);
            let out = (1..=2)
                .map(up)
                .chain((1..=8).flat_map(|r| {
                    let r = r as f32 * VOXEL_SIZE;
                    (0..8).map(move |i| Quat::from_rotation_y(i as f32 * TAU / 8.0) * Vec3::X * r)
                }))
                .chain((3..=16).map(up))
                .find(|step| !obstacles.collides(here + *step));
            match out {
                Some(step) => transform.translation += step,
                None => {
                    while obstacles.collides(transform.translation) {
                        transform.translation.y += VOXEL_SIZE;
                    }
                }
            }
            player.velocity = Vec3::ZERO;
            info!(
                "hero stepped out of something solid, to {}",
                transform.translation
            );
        }
        // The ground under the spawn point has loaded: it's in place.
        player.spawned = true;

        // A game moves the hero: glide to its spot, facing the way it goes (or jump
        // straight there from afar, as when a game holds the hero at startup).
        if let Some(spot) = pawned.0 {
            body.launch = None;
            if spot.distance(transform.translation) > 3.0 {
                transform.translation = spot;
            }
            let step = spot - transform.translation;
            if step.xz().length() > 0.02 {
                let goal = f32::atan2(-step.x, -step.z);
                let diff = (goal - player.facing + PI).rem_euclid(TAU) - PI;
                player.facing += diff * (1.0 - (-18.0 * dt).exp());
            }
            let pos = transform.translation + step * (1.0 - (-14.0 * dt).exp());
            player.velocity = (pos - transform.translation) / dt.max(1e-4);
            player.grounded = true;
            transform.translation = pos;
            transform.rotation = Quat::from_rotation_y(player.facing);
            let moving = player.velocity.xz().length();
            player.pace += (moving - player.pace) * (1.0 - (-10.0 * dt).exp());
            let pace = player.pace;
            player.stride = (player.stride + pace * dt * 3.2).rem_euclid(TAU);
            continue;
        }

        let pressed = |action: &str| !blocked.0 && binds.pressed(action);
        let axis = |pos, neg| pressed(pos) as i32 as f32 - pressed(neg) as i32 as f32;
        let input = Vec2::new(axis("right", "left"), axis("forward", "back")).clamp_length_max(1.0);
        // Camera-relative: forward is away from the camera.
        let forward = Vec3::new(-yaw.sin(), 0.0, -yaw.cos());
        let right = Vec3::new(-forward.z, 0.0, forward.x);
        let wish = right * input.x + forward * input.y;
        let speed = if pressed("run") { body.run } else { body.walk } * body.speed;

        let target = wish * speed;
        let accel = if player.grounded { 14.0 } else { 4.0 };
        let horizontal = player
            .velocity
            .xz()
            .lerp(target.xz(), 1.0 - (-accel * dt).exp());
        player.velocity.x = horizontal.x;
        player.velocity.z = horizontal.y;
        player.velocity.y -= body.fall * body.gravity * dt;
        // A game's player script moves the hero its own way (flying, swimming, dashing).
        let scripted_move = scripted.velocity.is_some();
        if let Some(v) = scripted.velocity {
            player.velocity = v;
        }
        if player.grounded {
            player.air_jumps = 0;
        }
        // The jump key, or a player script asking for the usual jump.
        let jump = (!blocked.0 && binds.just_pressed("jump")) || scripted.jump;
        if !scripted_move && jump && (player.grounded || player.air_jumps < body.air_jumps) {
            if !player.grounded {
                player.air_jumps += 1;
            }
            // Height goes with the square of speed: `jump` ×2 goes twice as high.
            player.velocity.y = body.jump_speed * (body.jump * body.gravity).sqrt();
        }
        if let Some(throw) = body.launch.take() {
            player.velocity = throw;
            player.grounded = false;
        }

        let heading = if scripted_move {
            player.velocity.with_y(0.0)
        } else {
            wish
        };
        if heading.length_squared() > 0.01 {
            let goal = f32::atan2(-heading.x, -heading.z);
            let diff = (goal - player.facing + PI).rem_euclid(TAU) - PI;
            player.facing += diff * (1.0 - (-12.0 * dt).exp());
        }

        // Axis-separated movement in small substeps against the voxel grid.
        let delta = player.velocity * dt;
        let steps = (delta.length() / (VOXEL_SIZE * 0.4)).ceil().max(1.0) as i32;
        let step = delta / steps as f32;
        let mut pos = transform.translation;
        let was_grounded = player.grounded;
        player.grounded = false;
        for _ in 0..steps {
            let next = pos + Vec3::Y * step.y;
            if obstacles.collides(next) {
                if step.y < 0.0 {
                    player.grounded = true;
                }
                player.velocity.y = 0.0;
            } else {
                pos = next;
            }
            for axis in [Vec3::X, Vec3::Z] {
                let next = pos + axis * step.dot(axis);
                if !obstacles.collides(next) {
                    pos = next;
                    continue;
                }
                // Walk up small ledges instead of stopping dead.
                let lift = [VOXEL_SIZE, STEP_HEIGHT]
                    .into_iter()
                    .map(|h| Vec3::Y * (h + 1e-3))
                    .find(|up| !obstacles.collides(pos + *up) && !obstacles.collides(next + *up));
                match lift {
                    Some(up) if was_grounded || player.grounded => {
                        pos = next + up;
                        player.lag -= up.y;
                    }
                    _ => {
                        if axis == Vec3::X {
                            player.velocity.x = 0.0;
                        } else {
                            player.velocity.z = 0.0;
                        }
                    }
                }
            }
        }
        if !player.grounded {
            player.grounded = obstacles.collides(pos - Vec3::Y * 0.02);
        }
        // Walk down small ledges too, rather than falling (and flipping to the jump pose) for a
        // few frames: settle onto ground within a step below by bisecting for the contact.
        if !player.grounded
            && was_grounded
            && player.velocity.y <= 0.0
            && obstacles.collides(pos - Vec3::Y * (STEP_HEIGHT + 0.02))
        {
            let (mut free, mut hit) = (0.0, STEP_HEIGHT + 0.02);
            for _ in 0..12 {
                let mid = (free + hit) * 0.5;
                if obstacles.collides(pos - Vec3::Y * mid) {
                    hit = mid;
                } else {
                    free = mid;
                }
            }
            pos.y -= free;
            player.lag += free;
            player.velocity.y = 0.0;
            player.grounded = true;
        }
        player.lag = player.lag.clamp(-STEP_HEIGHT, STEP_HEIGHT) * (-14.0 * dt).exp();
        transform.translation = pos;
        transform.rotation = Quat::from_rotation_y(player.facing);

        let moving = player.velocity.xz().length();
        player.pace += (moving - player.pace) * (1.0 - (-10.0 * dt).exp());
        let pace = player.pace;
        player.stride = (player.stride + pace * dt * 3.2).rem_euclid(TAU);
    }
}

fn animate_hero(
    time: Res<Time>,
    players: Query<&Player>,
    mut heroes: Query<&mut Transform, (With<Hero>, Without<Limb>)>,
    mut limbs: Query<(&Limb, &mut Transform), Without<Hero>>,
) {
    let Ok(player) = players.single() else {
        return;
    };
    for mut transform in &mut heroes {
        transform.translation.y = player.lag;
    }
    let walk = (player.pace / RUN_SPEED).clamp(0.0, 1.0);
    let swing = player.stride.sin() * (0.25 + 0.6 * walk) * (walk * 4.0).min(1.0);
    let breathe = (time.elapsed_secs() * 2.2).sin();
    let airborne = !player.grounded;
    for (limb, mut transform) in &mut limbs {
        let (angle, roll, offset) = match limb.name.as_str() {
            "leg_l" if airborne => (0.5, 0.0, Vec3::ZERO),
            "leg_r" if airborne => (-0.3, 0.0, Vec3::ZERO),
            "arm_l" if airborne => (-0.4, -0.9, Vec3::ZERO),
            "arm_r" if airborne => (0.2, 0.9, Vec3::ZERO),
            "leg_l" => (swing, 0.0, Vec3::ZERO),
            "leg_r" => (-swing, 0.0, Vec3::ZERO),
            "arm_l" => (-swing * 0.8, -0.08, Vec3::ZERO),
            "arm_r" => (swing * 0.8, 0.08, Vec3::ZERO),
            "torso" => {
                let bob = (player.stride * 2.0).cos().abs() * 0.03 * walk + breathe * 0.004;
                (-0.12 * walk, 0.0, Vec3::Y * bob)
            }
            "head" => (0.06 * walk + breathe * 0.015, 0.0, Vec3::ZERO),
            _ => continue,
        };
        transform.translation = limb.rest + offset;
        transform.rotation = Quat::from_rotation_x(angle) * Quat::from_rotation_z(roll);
    }
}

/// Everything solid the camera minds: terrain plus the builds near the hero.
struct Solids<'a> {
    world: &'a VoxelWorld,
    models: Vec<(&'a VoxelModel, Affine3A)>,
}

impl Solids<'_> {
    fn at(&self, p: Vec3) -> bool {
        self.world
            .get((p / VOXEL_SIZE).floor().as_ivec3())
            .is_some_and(is_solid)
            || self
                .models
                .iter()
                .any(|(model, inverse)| solid_at(model, inverse, p))
    }

    /// Along the ray from `origin`, within `max`: where it first meets something solid,
    /// and where the solid it has passed through adds up to [`THIN`] (one deep run, or
    /// many shallow ones, like a stepped slope seen along its length). The first
    /// [`SKIN`] of a run the ray starts inside (a leg in a slope, the hero against a
    /// wall) doesn't count; past that, the ray is inside the thing and it does.
    fn sight(&self, origin: Vec3, dir: Vec3, max: f32) -> (f32, f32) {
        let mut first = f32::INFINITY;
        let mut depth = 0.0;
        // At the start the ray may be inside the hero's own surroundings.
        let mut leaving = true;
        let mut t = 0.0;
        // A little past `max`, so a run starting just before it is measured.
        while t <= max + THIN {
            if self.at(origin + dir * t) {
                if !leaving {
                    first = first.min(t);
                    depth += STEP;
                    if depth >= THIN {
                        return (first, t);
                    }
                }
            } else {
                leaving = false;
            }
            t += STEP;
            leaving &= t < SKIN;
        }
        (first, f32::INFINITY)
    }

    /// Whether a camera at `p` looking along `-dir` has its near plane clear.
    fn room_at(&self, p: Vec3, right: Vec3, up: Vec3) -> bool {
        [
            (0.0, 0.0),
            (1.0, 1.0),
            (1.0, -1.0),
            (-1.0, 1.0),
            (-1.0, -1.0),
        ]
        .into_iter()
        .all(|(x, y)| !self.at(p + (right * x + up * y) * PROBE))
    }
}

/// Points on the hero the camera wants to see, across and up from the eyes (meters):
/// head, chest and legs, each at the middle and both sides.
const SIGHT: [(f32, f32); 9] = [
    (-0.25, 0.1),
    (0.0, 0.1),
    (0.25, 0.1),
    (-0.25, -0.5),
    (0.0, -0.5),
    (0.25, -0.5),
    (-0.25, -1.0),
    (0.0, -1.0),
    (0.25, -1.0),
];
/// How many of the [`SIGHT`] points may be hidden before the camera moves in. The rest
/// of the view is the dither's job: it fades whatever sits between camera and hero.
const MAY_HIDE: usize = 4;
/// Less solid than this along a sight line, all told (a branch, a post, a fence), doesn't
/// hide the hero, unless something hides every point at once (a thin wall).
const THIN: f32 = 0.8;
/// How much solid right at the hero a sight line forgives: what the hero stands in or
/// against, not between them and the camera.
const SKIN: f32 = 0.5;
/// How finely sight lines are walked.
const STEP: f32 = 0.1;

/// How far back from `pivot` along `dir` the camera can sit, up to `max`.
///
/// Nothing about what's in the way matters but its shape: the camera only moves in when
/// most of the hero would be hidden (something deep, or something that covers all of
/// them), and then just in front of it; past that, it only keeps out of solids itself.
fn clearance(solids: &Solids, pivot: Vec3, dir: Dir3, max: f32) -> f32 {
    let right = dir.cross(Vec3::Y).normalize_or(Vec3::X);
    let up = right.cross(*dir);
    let mut any = [0.0; SIGHT.len()];
    let mut thick = [0.0; SIGHT.len()];
    for (i, (x, y)) in SIGHT.into_iter().enumerate() {
        (any[i], thick[i]) = solids.sight(pivot + right * x + up * y, *dir, max);
    }
    thick.sort_by(f32::total_cmp);
    let covered = thick[MAY_HIDE].min(any.into_iter().fold(0.0, f32::max));
    let mut reach = if covered < max {
        (covered - MARGIN).max(0.0)
    } else {
        max
    };
    // Never inside anything: step in to the nearest spot with room.
    while reach > 0.0 && !solids.room_at(pivot + *dir * reach, right, up) {
        reach = (reach - STEP).max(0.0);
    }
    reach
}

pub fn follow_camera(
    time: Res<Time>,
    binds: Res<Bindings>,
    (motion, scroll): (Res<AccumulatedMouseMotion>, Res<AccumulatedMouseScroll>),
    over_ui: Res<PointerOverUi>,
    lock: Res<OrbitLock>,
    world: Res<VoxelWorld>,
    scripted: Res<ScriptedHero>,
    showcase: Res<Showcase>,
    (solids, areas): (
        Query<(&VoxelModel, &GlobalTransform), With<SolidModel>>,
        Res<SolidAreas>,
    ),
    mut cutout: ResMut<CameraCutout>,
    players: Query<(&Transform, &Player), Without<FollowCamera>>,
    mut heroes: Query<&mut Visibility, With<Hero>>,
    mut cursors: Query<&mut CursorOptions, With<PrimaryWindow>>,
    mut cameras: Query<(&mut FollowCamera, &mut Transform)>,
) {
    let rig = scripted.camera.unwrap_or_default();
    let Ok((player, hero)) = players.single() else {
        return;
    };
    let dt = time.delta_secs().min(1.0 / 20.0);
    let ease = |rate: f32| 1.0 - (-rate * dt).exp();
    for (mut cam, mut transform) in &mut cameras {
        // Turning follows the mouse directly, as in any third-person game.
        // While a drag selects ground, the button that selects doesn't also look around.
        let dragging = if lock.0 {
            binds.pressed_without("look", "click")
        } else {
            binds.pressed("look")
        };
        if showcase.0 {
            if cam.before.is_none() {
                cam.before = Some((cam.pitch, cam.distance));
                cam.crane = SHOWCASE_PITCH;
            }
            cam.yaw += dt * 0.07;
            cam.pitch += (cam.crane - cam.pitch) * ease(1.5);
            cam.distance += (10.0 - cam.distance) * ease(1.2);
        } else {
            // Back from the start screen: the view the player had.
            if let Some((pitch, distance)) = cam.before.take() {
                cam.distance = distance;
                cam.settle = Some(pitch);
            }
            if dragging {
                cam.settle = None;
                cam.yaw -= motion.delta.x * 0.005;
                cam.pitch = (cam.pitch + motion.delta.y * 0.004).clamp(-1.2, 1.45);
            } else if let Some(pitch) = cam.settle {
                cam.pitch += (pitch - cam.pitch) * ease(2.5);
                if (pitch - cam.pitch).abs() < 0.005 {
                    cam.settle = None;
                }
            }
        }
        // Once a press turns into a drag, hold the cursor in place (a click stays a click).
        #[cfg(not(target_arch = "wasm32"))]
        {
            let grab =
                dragging && (cam.grabbed || (!over_ui.0 && motion.delta.length_squared() > 4.0));
            if grab != cam.grabbed
                && let Ok(mut cursor) = cursors.single_mut()
            {
                use bevy::window::CursorGrabMode;
                cursor.grab_mode = if grab {
                    CursorGrabMode::Locked
                } else {
                    CursorGrabMode::None
                };
                cursor.visible = !grab;
                cam.grabbed = grab;
            }
        }
        #[cfg(target_arch = "wasm32")]
        let _ = &mut cursors;

        let lines = if over_ui.0 {
            0.0
        } else {
            match scroll.unit {
                MouseScrollUnit::Line => scroll.delta.y,
                MouseScrollUnit::Pixel => scroll.delta.y / 40.0,
            }
        };
        cam.distance = (cam.distance * (1.0 - lines * 0.08)).clamp(2.0, 25.0);

        // Track the eased model, not the body, so steps don't jolt the view; follow
        // tightly across the ground and a little softer up and down.
        let goal = player.translation + Vec3::Y * (EYE + hero.lag + rig.height.unwrap_or(0.0));
        if cam.target == Vec3::ZERO || cam.target.distance(goal) > 8.0 {
            cam.target = goal;
        }
        let (across, rise) = (ease(25.0), ease(12.0));
        cam.target = Vec3::new(
            cam.target.x.lerp(goal.x, across),
            cam.target.y.lerp(goal.y, rise),
            cam.target.z.lerp(goal.z, across),
        );

        // A game's player script may hold the camera its own way.
        if let Some(yaw) = rig.yaw {
            cam.yaw = yaw;
        }
        let pitch = rig.pitch.unwrap_or(cam.pitch);
        let wanted = rig.distance.unwrap_or(cam.distance);
        let wanted = if wanted < 0.5 { FIRST_PERSON } else { wanted };
        cam.zoom += (wanted - cam.zoom) * ease(10.0);

        let rotation = Quat::from_euler(EulerRot::YXZ, cam.yaw, -pitch, 0.0);
        let dir = Dir3::new_unchecked((rotation * Vec3::Z).normalize());
        // Pull in front of anything between the camera and the hero: quickly, so the
        // view rarely sees inside a wall, but eased rather than a jump; then drift back out.
        let around = (cam.zoom > 0.0).then(|| {
            let r = cam.zoom + THIN + 1.0;
            let models = areas
                .0
                .within(
                    [cam.target.x - r, cam.target.z - r],
                    [cam.target.x + r, cam.target.z + r],
                )
                .into_iter()
                .filter_map(|entity| solids.get(entity).ok())
                .map(|(model, transform)| (model, transform.affine().inverse()))
                .collect();
            Solids {
                world: &world,
                models,
            }
        });
        let limit = around.as_ref().map_or(cam.zoom, |solids| {
            clearance(solids, cam.target, dir, cam.zoom)
        });
        // The start screen's camera looks where it's heading: with a hill in the way, or
        // soon to be, it climbs until it sees over, and comes back down once past.
        if showcase.0
            && let Some(solids) = &around
        {
            let (yaw, zoom, target) = (cam.yaw, cam.zoom, cam.target);
            let blocked = |pitch: f32| {
                [0.0, 0.2, 0.4].into_iter().any(|ahead| {
                    let rotation = Quat::from_euler(EulerRot::YXZ, yaw + ahead, -pitch, 0.0);
                    let dir = Dir3::new_unchecked((rotation * Vec3::Z).normalize());
                    clearance(solids, target, dir, zoom) < zoom - 0.5
                })
            };
            if blocked(cam.crane) {
                cam.crane = (cam.crane + dt * 0.9).min(SHOWCASE_STEEPEST);
            } else if cam.crane > SHOWCASE_PITCH && !blocked(cam.crane - 0.15) {
                cam.crane = (cam.crane - dt * 0.25).max(SHOWCASE_PITCH);
            }
        }
        let wanted_pull = cam.zoom - limit;
        // Moving in is quick, so the view rarely sees inside a wall; gentler on the start
        // screen, where the climb above does most of it.
        let rate = if wanted_pull <= cam.pull {
            3.5
        } else if showcase.0 {
            5.0
        } else {
            16.0
        };
        cam.pull += (wanted_pull - cam.pull) * ease(rate);
        cam.pull = cam.pull.clamp(0.0, cam.zoom.max(0.0));
        let reach = cam.zoom - cam.pull;

        // Up close the hero would fill the view from inside: hide them.
        if let Ok(mut visibility) = heroes.single_mut() {
            visibility.set_if_neq(if reach > 0.6 {
                Visibility::Inherited
            } else {
                Visibility::Hidden
            });
        }

        let shake = if rig.shake > 0.0 {
            let t = time.elapsed_secs() * 37.0;
            Vec3::new(t.sin(), (t * 1.3).cos(), (t * 0.7).sin()) * rig.shake * 0.3
        } else {
            Vec3::ZERO
        };
        transform.translation = cam.target + *dir * reach + shake;
        transform.look_to(-*dir, Vec3::Y);

        // Whatever is still between camera and hero is dithered away, not moved in front of.
        let strength = if reach > 0.6 { 1.0 } else { 0.0 };
        cutout.set_if_neq(CameraCutout {
            camera: transform.translation,
            hero: cam.target - Vec3::Y * CUTOUT_DROP,
            // Up close the cut would fill the screen; keep it about the hero's size there.
            radius: CUTOUT_RADIUS.min(reach * 0.5),
            strength: cutout.strength + (strength - cutout.strength) * ease(8.0),
        });
    }
}
