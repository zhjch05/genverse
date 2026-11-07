//! The hero: a voxel character assembled from model parts, a kinematic controller with
//! voxel collision, procedural walk animation, and a third-person follow camera.

use std::f32::consts::{PI, TAU};

use bevy::{
    input::mouse::{AccumulatedMouseMotion, AccumulatedMouseScroll, MouseScrollUnit},
    platform::collections::HashMap,
    prelude::*,
};

use crate::{
    voxel::{VoxelModel, VoxelModelAsset, query::overlaps_box},
    world::{
        ChunkLoader, SolidModel, VoxelWorld,
        terrain::{SEA_LEVEL, VOXEL_SIZE, is_solid},
    },
};

const HALF_WIDTH: f32 = 0.28;
const HEIGHT: f32 = 1.35;
const WALK_SPEED: f32 = 4.0;
const RUN_SPEED: f32 = 7.0;
const JUMP_SPEED: f32 = 7.5;
const GRAVITY: f32 = 24.0;
/// Highest ledge the hero walks up without jumping.
const STEP_HEIGHT: f32 = 2.0 * VOXEL_SIZE;

pub struct PlayerPlugin;

impl Plugin for PlayerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<InputBlocked>()
            .init_resource::<PointerOverUi>()
            .init_resource::<OrbitLock>()
            .add_systems(Startup, spawn_player)
            .add_systems(
                Update,
                (
                    build_hero_parts,
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

/// Set while the pointer is over a UI, whose scrolling isn't the camera's zoom.
#[derive(Resource, Default)]
pub struct PointerOverUi(pub bool);

/// While set, left-drag no longer turns the camera (another tool owns it); right-drag still does.
#[derive(Resource, Default)]
pub struct OrbitLock(pub bool);

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
}

#[derive(Component)]
struct Hero {
    model: Handle<VoxelModelAsset>,
    parts: HashMap<String, Entity>,
}

impl Player {
    /// True once the ground under the spawn point has loaded and the hero stands on it.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))] // Used by the desktop-only builder.
    pub fn has_landed(&self) -> bool {
        self.spawned && self.grounded
    }
}

/// One animated limb; `rest` is its translation relative to its parent joint.
#[derive(Component)]
struct Limb {
    name: String,
    rest: Vec3,
}

#[derive(Component)]
pub struct FollowCamera {
    yaw: f32,
    pitch: f32,
    distance: f32,
    target: Vec3,
}

impl Default for FollowCamera {
    fn default() -> Self {
        let env = |k: &str, d: f32| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        Self {
            yaw: env("CUBEWISH_YAW", 0.6),
            pitch: env("CUBEWISH_PITCH", 0.35),
            distance: env("CUBEWISH_DIST", 6.5),
            target: Vec3::ZERO,
        }
    }
}

fn spawn_player(mut commands: Commands, asset_server: Res<AssetServer>, world: Res<VoxelWorld>) {
    // Walk outward from the origin to the first dry column. `CUBEWISH_SPAWN=x,z` (meters)
    // starts elsewhere, e.g. to keep test players apart.
    let terrain = world.terrain;
    let origin = std::env::var("CUBEWISH_SPAWN")
        .ok()
        .and_then(|v| v.split_once(',').and_then(|(x, z)| Some((x.trim().parse::<f32>().ok()?, z.trim().parse::<f32>().ok()?))))
        .map_or(IVec2::ZERO, |(x, z)| (Vec2::new(x, z) / VOXEL_SIZE).as_ivec2());
    let (mut x, mut z) = (origin.x, origin.y);
    for i in 0..400 {
        let angle = i as f32 * 2.4;
        let r = (i as f32).sqrt() * 6.0;
        (x, z) = (origin.x + (angle.cos() * r) as i32, origin.y + (angle.sin() * r) as i32);
        if terrain.height(x, z) > SEA_LEVEL + 2 {
            break;
        }
    }
    let ground = terrain.height(x, z) + 1;
    let start = Vec3::new(x as f32 + 0.5, ground as f32 + 1.0, z as f32 + 0.5) * VOXEL_SIZE;

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
                        Limb { name: part.name.clone(), rest },
                        Transform::from_translation(rest),
                        ChildOf(parent_entity),
                    ))
                    .id();
                hero.parts.insert(part.name.clone(), entity);
                false
            });
            if pending.len() == before {
                warn!("hero parts have unknown or cyclic parents: {:?}", pending.iter().map(|p| &p.name).collect::<Vec<_>>());
                break;
            }
        }
    }
}

/// The hero's collision box for feet at `feet`, as `(min, max)`.
pub fn bounds(feet: Vec3) -> (Vec3, Vec3) {
    (feet - Vec3::new(HALF_WIDTH, 0.0, HALF_WIDTH), feet + Vec3::new(HALF_WIDTH, HEIGHT, HALF_WIDTH))
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
        self.models.iter().any(|(model, transform)| overlaps_box(model, transform, lo, hi))
    }
}

fn move_player(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    blocked: Res<InputBlocked>,
    world: Res<VoxelWorld>,
    cameras: Query<&FollowCamera>,
    solids: Query<(&VoxelModel, &GlobalTransform), With<SolidModel>>,
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
            models: solids
                .iter()
                .filter(|(model, t)| {
                    let reach = model.grid.size().as_vec3().length() * model.voxel_size + 2.0;
                    t.translation().distance(here) < reach
                })
                .collect(),
        };
        // Drop into place once the ground under the spawn point has loaded.
        if !player.spawned {
            while obstacles.collides(transform.translation) {
                transform.translation.y += VOXEL_SIZE;
            }
            player.spawned = true;
        }

        let pressed = |key| !blocked.0 && keys.pressed(key);
        let axis = |pos, neg| pressed(pos) as i32 as f32 - pressed(neg) as i32 as f32;
        let input = Vec2::new(
            axis(KeyCode::KeyD, KeyCode::KeyA) + axis(KeyCode::ArrowRight, KeyCode::ArrowLeft),
            axis(KeyCode::KeyW, KeyCode::KeyS) + axis(KeyCode::ArrowUp, KeyCode::ArrowDown),
        )
        .clamp_length_max(1.0);
        // Camera-relative: forward is away from the camera.
        let forward = Vec3::new(-yaw.sin(), 0.0, -yaw.cos());
        let right = Vec3::new(-forward.z, 0.0, forward.x);
        let wish = right * input.x + forward * input.y;
        let speed = if pressed(KeyCode::ShiftLeft) { RUN_SPEED } else { WALK_SPEED };

        let target = wish * speed;
        let accel = if player.grounded { 14.0 } else { 4.0 };
        let horizontal = player.velocity.xz().lerp(target.xz(), 1.0 - (-accel * dt).exp());
        player.velocity.x = horizontal.x;
        player.velocity.z = horizontal.y;
        player.velocity.y -= GRAVITY * dt;
        if player.grounded && !blocked.0 && keys.just_pressed(KeyCode::Space) {
            player.velocity.y = JUMP_SPEED;
        }

        if wish.length_squared() > 0.01 {
            let goal = f32::atan2(-wish.x, -wish.z);
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
        if !player.grounded && was_grounded && player.velocity.y <= 0.0
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

fn follow_camera(
    time: Res<Time>,
    buttons: Res<ButtonInput<MouseButton>>,
    motion: Res<AccumulatedMouseMotion>,
    scroll: Res<AccumulatedMouseScroll>,
    over_ui: Res<PointerOverUi>,
    lock: Res<OrbitLock>,
    world: Res<VoxelWorld>,
    players: Query<(&Transform, &Player), Without<FollowCamera>>,
    mut cameras: Query<(&mut FollowCamera, &mut Transform)>,
) {
    let Ok((player, hero)) = players.single() else {
        return;
    };
    let dt = time.delta_secs();
    for (mut cam, mut transform) in &mut cameras {
        if (buttons.pressed(MouseButton::Left) && !lock.0) || buttons.pressed(MouseButton::Right) {
            cam.yaw -= motion.delta.x * 0.005;
            cam.pitch = (cam.pitch + motion.delta.y * 0.004).clamp(-0.3, 1.3);
        }
        let lines = if over_ui.0 {
            0.0
        } else {
            match scroll.unit {
            MouseScrollUnit::Line => scroll.delta.y,
            MouseScrollUnit::Pixel => scroll.delta.y / 40.0,
            }
        };
        cam.distance = (cam.distance * (1.0 - lines * 0.08)).clamp(2.0, 25.0);

        // Track the eased model, not the body, so steps don't jolt the view.
        let goal = player.translation + Vec3::Y * (1.1 + hero.lag);
        if cam.target == Vec3::ZERO {
            cam.target = goal;
        }
        let blend = 1.0 - (-10.0 * dt).exp();
        cam.target = cam.target.lerp(goal, blend);

        // Pull the camera in front of any terrain between it and the hero.
        let rotation = Quat::from_euler(EulerRot::YXZ, cam.yaw, -cam.pitch, 0.0);
        let dir = rotation * Vec3::Z;
        let mut distance = cam.distance;
        let mut d = 0.3;
        while d < cam.distance {
            let p = VoxelWorld::voxel_at(cam.target + dir * d);
            if world.get(p).is_some_and(is_solid) {
                distance = (d - 0.3).max(0.5);
                break;
            }
            d += VOXEL_SIZE * 0.5;
        }
        transform.translation = cam.target + dir * distance;
        transform.look_at(cam.target, Vec3::Y);
    }
}
