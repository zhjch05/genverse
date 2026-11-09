//! Shows what games ask for ([`cubewish_rules::logic::present`]): bursts of particles,
//! sounds made from numbers, the sky (time of day, fog, sun, rain or snow), and heroes
//! shown as one of a game's models. Effects and sounds play once each, near the hero;
//! the sky and looks last while the game says so.

use std::f32::consts::{PI, TAU};

use bevy::{
    audio::Volume,
    pbr::{DistanceFog, FogFalloff},
    prelude::*,
};
use cubewish_rules::{
    build::{BuildLimits, compile},
    logic::present::{Fx, Shown, Sound},
};

use crate::{
    player::{Hero, Player},
    voxel::VoxelModel,
    world::{ledger::Ledger, lod::VIEW_DISTANCE},
};

/// Effects and sounds a player script made this frame, for this player alone.
#[derive(Resource, Default)]
pub struct LocalShows(pub Vec<([f32; 3], Shown)>);

/// How far from the hero effects and sounds still play, meters.
const HEARING: f32 = 64.0;
/// The most particles alive at once.
const MAX_PARTICLES: usize = 2000;

#[derive(Component)]
pub struct Particle {
    velocity: Vec3,
    gravity: f32,
    age: f32,
    life: f32,
    size: f32,
}

/// A burst's particles share a material, which fades from one color to the next.
#[derive(Component)]
pub struct Burst {
    material: Handle<StandardMaterial>,
    from: LinearRgba,
    to: LinearRgba,
    glow: bool,
    age: f32,
    life: f32,
}

fn color(hex: &str) -> Color {
    Srgba::hex(hex.trim_start_matches('#')).map(Color::from).unwrap_or(Color::WHITE)
}

/// Plays the effects and sounds the games made since we last looked, near the hero.
#[allow(clippy::too_many_arguments)]
pub fn play_shows(
    mut commands: Commands,
    ledger: Res<Ledger>,
    mut local: ResMut<LocalShows>,
    mut seen: Local<(String, u64)>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut sounds: ResMut<Assets<AudioSource>>,
    particles: Query<(), With<Particle>>,
    heroes: Query<&Transform, With<Player>>,
) {
    let world = super::worlds::current_title(&ledger);
    let shows = ledger.logic.state.shows();
    let newest = shows.last().map_or(0, |s| s.change);
    let me = &ledger.profile.id;
    let mut now: Vec<([f32; 3], Shown)> = std::mem::take(&mut local.0);
    if seen.0 != world {
        // Just arrived: what happened before is history.
        *seen = (world, newest);
    } else {
        now.extend(shows.iter().filter(|s| s.change > seen.1 && s.to.as_ref().is_none_or(|t| t == me)).map(|s| (s.pos, s.what.clone())));
        seen.1 = seen.1.max(newest);
    }
    let Ok(hero) = heroes.single() else { return };
    let mut alive = particles.iter().count();
    for (pos, what) in now {
        let at = Vec3::from_array(pos);
        let distance = at.distance(hero.translation);
        if distance > HEARING {
            continue;
        }
        match what {
            Shown::Effect(fx) => {
                alive += burst(&mut commands, &mut meshes, &mut materials, at, &fx, MAX_PARTICLES.saturating_sub(alive));
            }
            Shown::Sound(sound) => play_sound(&mut commands, &mut sounds, &sound, 1.0 - distance / HEARING),
        }
    }
}

fn burst(commands: &mut Commands, meshes: &mut Assets<Mesh>, materials: &mut Assets<StandardMaterial>, at: Vec3, fx: &Fx, room: usize) -> usize {
    let count = (fx.count as usize).min(room);
    if count == 0 {
        return 0;
    }
    let from = color(&fx.color).to_linear();
    let to = fx.to.as_deref().map_or(from, |c| color(c).to_linear());
    let material = materials.add(StandardMaterial {
        base_color: from.into(),
        emissive: if fx.glow { from * 4.0 } else { LinearRgba::BLACK },
        unlit: fx.glow,
        ..default()
    });
    let mesh = meshes.add(Cuboid::from_length(1.0));
    let parent = commands
        .spawn((
            Name::new("Effect"),
            Burst { material: material.clone(), from, to, glow: fx.glow, age: 0.0, life: fx.life },
            Transform::from_translation(at),
            Visibility::default(),
        ))
        .id();
    // Spread directions evenly over a cone that opens from a jet (0) to every way (1).
    let golden = PI * (3.0 - 5f32.sqrt());
    for i in 0..count {
        let t = (i as f32 + 0.5) / count as f32;
        let y = 1.0 - t * (1.0 + fx.spread);
        let r = (1.0 - y * y).max(0.0).sqrt();
        let a = i as f32 * golden;
        let dir = Vec3::new(r * a.cos(), y, r * a.sin()).normalize_or_zero();
        let speed = fx.speed * (0.6 + 0.4 * ((i * 7919) % 97) as f32 / 97.0);
        commands.spawn((
            Particle { velocity: dir * speed, gravity: fx.gravity, age: 0.0, life: fx.life, size: fx.size },
            Mesh3d(mesh.clone()),
            MeshMaterial3d(material.clone()),
            Transform::from_scale(Vec3::splat(fx.size)),
            ChildOf(parent),
        ));
    }
    count
}

fn play_sound(commands: &mut Commands, sounds: &mut Assets<AudioSource>, sound: &Sound, loudness: f32) {
    let source = sounds.add(AudioSource { bytes: sound.wav().into() });
    commands.spawn((AudioPlayer(source), PlaybackSettings::DESPAWN.with_volume(Volume::Linear(loudness.clamp(0.0, 1.0)))));
}

/// Moves, fades and shrinks particles, and lets them go when they're done.
pub fn update_particles(
    mut commands: Commands,
    time: Res<Time>,
    mut particles: Query<(Entity, &mut Particle, &mut Transform), Without<Burst>>,
    mut bursts: Query<(Entity, &mut Burst, Option<&Children>)>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let dt = time.delta_secs();
    for (entity, mut p, mut transform) in &mut particles {
        p.age += dt;
        p.velocity.y -= p.gravity * dt;
        let step = p.velocity * dt;
        transform.translation += step;
        transform.scale = Vec3::splat(p.size * (1.0 - p.age / p.life).clamp(0.0, 1.0));
        if p.age >= p.life {
            commands.entity(entity).despawn();
        }
    }
    for (entity, mut burst, children) in &mut bursts {
        burst.age += dt;
        let t = (burst.age / burst.life).clamp(0.0, 1.0);
        if let Some(mut m) = materials.get_mut(&burst.material) {
            let c = burst.from * (1.0 - t) + burst.to * t;
            m.base_color = c.into();
            if burst.glow {
                m.emissive = c * 4.0;
            }
        }
        if burst.age >= burst.life || children.is_none_or(|c| c.is_empty()) && burst.age > 0.1 {
            commands.entity(entity).despawn();
        }
    }
}

/// The sun and fog as a game has them (and its rain or snow around the hero), or as
/// the world has them.
#[allow(clippy::too_many_arguments)]
pub fn apply_sky(
    mut commands: Commands,
    time: Res<Time>,
    ledger: Res<Ledger>,
    mut base: Local<Option<(Quat, f32, Color)>>,
    mut suns: Query<(&mut DirectionalLight, &mut Transform)>,
    mut fogs: Query<&mut DistanceFog>,
    heroes: Query<&Transform, (With<Player>, Without<DirectionalLight>)>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut weather: Local<Option<(Handle<Mesh>, Handle<StandardMaterial>, Handle<StandardMaterial>)>>,
    particles: Query<(), With<Particle>>,
) {
    let sky = ledger.logic.state.sky().cloned().unwrap_or_default();
    let Ok((mut sun, mut sun_at)) = suns.single_mut() else { return };
    let Ok(mut fog) = fogs.single_mut() else { return };
    let (rotation, illuminance, fog_color) = *base.get_or_insert((sun_at.rotation, sun.illuminance, fog.color));
    sun_at.rotation = match sky.time {
        // Noon high in the south, the horizon at 0.25 and 0.75, below it at night.
        Some(t) => Quat::from_euler(EulerRot::YXZ, PI * 0.35, -((t - 0.25) * TAU).sin() * 1.3, 0.0),
        None => rotation,
    };
    let night = sky.time.map_or(1.0, |t| ((t - 0.25) * TAU).sin().clamp(0.0, 1.0).sqrt());
    sun.illuminance = illuminance * sky.sun.unwrap_or(1.0) * night.max(0.02);
    fog.color = sky.fog_color.as_deref().map_or(fog_color, color);
    let thick = sky.fog.unwrap_or(0.0);
    let end = VIEW_DISTANCE * 0.98 * (1.0 - thick * 0.95);
    fog.falloff = FogFalloff::Linear { start: end * if thick > 0.0 { 0.05 } else { 0.3 }, end };

    // Rain or snow falls around the hero.
    let Some(kind) = sky.weather.as_deref() else { return };
    let Ok(hero) = heroes.single() else { return };
    let (mesh, rain, snow) = weather.get_or_insert_with(|| {
        let mut drop = |c: Color| materials.add(StandardMaterial { base_color: c, unlit: true, alpha_mode: AlphaMode::Blend, ..default() });
        (meshes.add(Cuboid::from_length(1.0)), drop(Color::srgba(0.7, 0.8, 1.0, 0.6)), drop(Color::srgba(1.0, 1.0, 1.0, 0.9)))
    });
    if particles.iter().count() >= MAX_PARTICLES {
        return;
    }
    let snowing = kind == "snow";
    let per_second = if snowing { 120.0 } else { 400.0 };
    let n = (per_second * time.delta_secs()).ceil() as usize;
    let seed = (time.elapsed_secs() * 1000.0) as u32;
    for i in 0..n {
        let hash = |k: u32| ((seed.wrapping_mul(2654435761) ^ (i as u32 + k).wrapping_mul(40503)) % 1000) as f32 / 1000.0;
        let offset = Vec3::new(hash(1) * 40.0 - 20.0, 10.0 + hash(2) * 4.0, hash(3) * 40.0 - 20.0);
        let (velocity, size, material) = if snowing {
            (Vec3::new(hash(4) - 0.5, -1.5, hash(5) - 0.5), Vec3::splat(0.06), snow.clone())
        } else {
            (Vec3::new(0.0, -14.0, 0.0), Vec3::new(0.02, 0.35, 0.02), rain.clone())
        };
        commands.spawn((
            Particle { velocity, gravity: 0.0, age: 0.0, life: if snowing { 8.0 } else { 1.0 }, size: size.x },
            Mesh3d(mesh.clone()),
            MeshMaterial3d(material),
            Transform::from_translation(hero.translation + offset).with_scale(size),
        ));
    }
}

/// The hero, as a game shows them: one of its models in place of the usual one.
#[derive(Component)]
pub struct Costume(String);

pub fn dress_hero(
    mut commands: Commands,
    ledger: Res<Ledger>,
    players: Query<Entity, With<Player>>,
    mut heroes: Query<&mut Visibility, With<Hero>>,
    costumes: Query<(Entity, &Costume)>,
) {
    let logic = &ledger.logic;
    let wanted = logic.state.appearance(&ledger.profile.id).and_then(|a| {
        let install = logic.installed.get(&a.package)?;
        let spec = logic.packages.get(&install.package.hash)?.models.get(&a.model)?;
        Some((format!("{}/{}/{}/{}", install.package.hash, a.model, a.scale, spec.name), spec, a.scale))
    });
    let current = costumes.single().ok();
    if current.map(|(_, c)| c.0.as_str()) == wanted.as_ref().map(|w| w.0.as_str()) {
        return;
    }
    if let Some((entity, _)) = current {
        commands.entity(entity).despawn();
    }
    let Ok(player) = players.single() else { return };
    let shown = match wanted {
        Some((key, spec, scale)) => match compile(spec, &BuildLimits::ACCEPTED) {
            Ok(build) => {
                commands.spawn((
                    Name::new("Costume"),
                    Costume(key),
                    VoxelModel::from(&build.model),
                    Transform::from_rotation(Quat::from_rotation_y(PI)).with_scale(Vec3::splat(scale)),
                    Visibility::default(),
                    ChildOf(player),
                ));
                Visibility::Hidden
            }
            Err(_) => Visibility::Inherited,
        },
        None => Visibility::Inherited,
    };
    for mut v in &mut heroes {
        *v = shown;
    }
}
