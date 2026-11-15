//! Presentation: physical sky, sun with cascaded shadows, distance fog that hides
//! chunk streaming, and the post-processing stack on the follow camera (which effects
//! run is up to [`crate::graphics`]).

use std::f32::consts::PI;

use bevy::{
    camera::Exposure,
    light::{
        Atmosphere, AtmosphereEnvironmentMapLight, atmosphere::ScatteringMedium, light_consts::lux,
    },
    pbr::{AtmosphereSettings, DistanceFog, FogFalloff},
    prelude::*,
    render::view::{
        Msaa, Tonemapping,
        screenshot::{Screenshot, save_to_disk},
    },
};

use genverse_rules::terrain::{VOXEL_SIZE, block};

use crate::{
    player::FollowCamera,
    world::{VoxelWorld, lod::VIEW_DISTANCE},
};

pub struct ScenePlugin;

impl Plugin for ScenePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, setup)
            // After the sky (and a game's fog) is set for the frame, which it overrides.
            .add_systems(PostUpdate, underwater);
        // `GENVERSE_SHOT=out.png [GENVERSE_YAW=0.5] cargo run` renders one still and exits.
        if let Ok(path) = std::env::var("GENVERSE_SHOT") {
            app.insert_resource(StillShot { path, frame: 0 })
                .add_systems(Update, take_still_shot);
        }
    }
}

#[derive(Resource)]
struct StillShot {
    path: String,
    frame: u32,
}

/// Seconds to wait before the still; `GENVERSE_SHOT_AFTER` overrides it.
fn shot_delay() -> f32 {
    std::env::var("GENVERSE_SHOT_AFTER")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7.0)
}

fn setup(mut commands: Commands, mut media: ResMut<Assets<ScatteringMedium>>) {
    // Sink the planet surface far below the world so, past the streamed area,
    // the view fades into haze instead of showing a flat ground plane.
    let atmosphere = Atmosphere::earth(media.add(ScatteringMedium::default()));
    let altitude = 2_000.0;
    commands.spawn((
        Name::new("Sky"),
        Transform::from_xyz(0.0, -(atmosphere.inner_radius + altitude), 0.0),
        atmosphere,
    ));

    // Mid-afternoon sun: warm, high enough for crisp shadows under trees.
    commands.spawn((
        Name::new("Sun"),
        DirectionalLight {
            illuminance: lux::RAW_SUNLIGHT,
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::YXZ, PI * 0.35, -0.75, 0.0)),
    ));

    // Haze over the coarse terrain, fading it out just before the streaming edge.
    let view = VIEW_DISTANCE;
    commands.spawn((
        Name::new("Camera"),
        Camera3d::default(),
        Msaa::Off,
        AtmosphereSettings::default(),
        AtmosphereEnvironmentMapLight::default(),
        DistanceFog {
            color: Color::srgb(0.66, 0.74, 0.84),
            directional_light_color: Color::srgba(1.0, 0.9, 0.7, 0.3),
            directional_light_exponent: 20.0,
            falloff: FogFalloff::Linear {
                start: view * 0.3,
                end: view * 0.98,
            },
        },
        Exposure { ev100: 13.5 },
        Tonemapping::TonyMcMapface,
        FollowCamera::default(),
    ));

    // On desktop the builder draws these, with what applies right now.
    #[cfg(target_arch = "wasm32")]
    commands.spawn((
        Text::new("WASD: move   Shift: run   Space: jump   Drag: look   Scroll: zoom"),
        TextFont {
            font_size: FontSize::Px(14.0),
            ..default()
        },
        TextColor(Color::srgba(1.0, 1.0, 1.0, 0.75)),
        Node {
            position_type: PositionType::Absolute,
            left: px(12),
            bottom: px(10),
            ..default()
        },
    ));
}

/// With the camera under water, the view is the water's: a short haze of its color
/// instead of the sky's, so the sea floor reads as under the sea rather than as dry
/// sand (the surface, seen from below, barely shows).
fn underwater(
    world: Res<VoxelWorld>,
    mut above: Local<Option<DistanceFog>>,
    mut cameras: Query<(&Transform, &mut DistanceFog), With<FollowCamera>>,
) {
    let Ok((at, mut fog)) = cameras.single_mut() else {
        return;
    };
    let voxel = VoxelWorld::voxel_at(at.translation);
    if world.get(voxel) != Some(block::WATER) {
        // Surfaced: the air's fog again.
        if let Some(air) = above.take() {
            *fog = air;
        }
        return;
    }
    // The sky sets the air's fog afresh each frame, before this; the first frame under
    // is the one to keep.
    if above.is_none() {
        *above = Some(fog.clone());
    }
    let water = world
        .color_of(block::WATER)
        .unwrap_or(Color::srgb(0.29, 0.64, 0.77))
        .to_srgba();
    // The haze is about as bright as the sunlit ground it covers (the scene is lit in
    // HDR, so a dark haze would barely show); deeper is a little darker, and thicker.
    let depth = (1..64)
        .take_while(|dy| world.get(voxel + IVec3::Y * *dy) == Some(block::WATER))
        .count() as f32
        * VOXEL_SIZE;
    let shade = 1.1 / (1.0 + depth * 0.04);
    fog.color = Color::srgb(water.red * shade, water.green * shade, water.blue * shade);
    fog.directional_light_color = Color::NONE;
    fog.falloff = FogFalloff::Exponential {
        density: 0.22 + depth * 0.01,
    };
}

fn take_still_shot(
    time: Res<Time>,
    mut commands: Commands,
    mut shot: ResMut<StillShot>,
    mut exit: MessageWriter<AppExit>,
) {
    // Give chunks time to stream in and TAA history time to settle, then wait a
    // few frames for the capture to land before exiting.
    if shot.frame == 0 && time.elapsed_secs() < shot_delay() {
        return;
    }
    shot.frame += 1;
    if shot.frame == 1 {
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(shot.path.clone()));
    }
    if shot.frame == 30 {
        exit.write(AppExit::Success);
    }
}
