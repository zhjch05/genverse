//! Presentation: physical sky, sun with cascaded shadows, distance fog that hides
//! chunk streaming, and the post-processing stack on the follow camera.

use std::f32::consts::PI;

use bevy::{
    anti_alias::taa::TemporalAntiAliasing,
    camera::Exposure,
    light::{
        Atmosphere, AtmosphereEnvironmentMapLight, CascadeShadowConfigBuilder,
        ShadowFilteringMethod, atmosphere::ScatteringMedium, light_consts::lux,
    },
    pbr::{AtmosphereSettings, DistanceFog, FogFalloff, ScreenSpaceAmbientOcclusion},
    post_process::bloom::Bloom,
    prelude::*,
    render::view::{
        Msaa, Tonemapping,
        screenshot::{Screenshot, save_to_disk},
    },
};

use crate::{
    player::FollowCamera,
    world::{CHUNK, VIEW_RADIUS, terrain::VOXEL_SIZE},
};

pub struct ScenePlugin;

impl Plugin for ScenePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, setup);
        // `CUBELARK_SHOT=out.png [CUBELARK_YAW=0.5] cargo run` renders one still and exits.
        if let Ok(path) = std::env::var("CUBELARK_SHOT") {
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

/// Seconds to wait before the still; `CUBELARK_SHOT_AFTER` overrides it.
fn shot_delay() -> f32 {
    std::env::var("CUBELARK_SHOT_AFTER").ok().and_then(|v| v.parse().ok()).unwrap_or(7.0)
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
            contact_shadows_enabled: true,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::YXZ, PI * 0.35, -0.75, 0.0)),
        CascadeShadowConfigBuilder {
            num_cascades: 4,
            first_cascade_far_bound: 8.0,
            maximum_distance: 70.0,
            ..default()
        }
        .build(),
    ));

    // Fade the terrain out just before the streaming edge.
    let view = (VIEW_RADIUS * CHUNK) as f32 * VOXEL_SIZE;
    let mut camera = commands.spawn((
        Name::new("Camera"),
        Camera3d::default(),
        Msaa::Off,
        TemporalAntiAliasing::default(),
        ShadowFilteringMethod::Temporal,
        AtmosphereSettings::default(),
        AtmosphereEnvironmentMapLight::default(),
        DistanceFog {
            color: Color::srgb(0.66, 0.74, 0.84),
            directional_light_color: Color::srgba(1.0, 0.9, 0.7, 0.3),
            directional_light_exponent: 20.0,
            falloff: FogFalloff::Linear {
                start: view * 0.45,
                end: view * 0.98,
            },
        },
        Exposure { ev100: 13.5 },
        Tonemapping::TonyMcMapface,
        Bloom::NATURAL,
        FollowCamera::default(),
    ));
    // SSAO isn't supported on WebGPU yet; baked voxel AO still applies there.
    if cfg!(not(target_arch = "wasm32")) {
        camera.insert(ScreenSpaceAmbientOcclusion::default());
    }

    commands.spawn((
        Text::new(if cfg!(target_arch = "wasm32") {
            "WASD: move   Shift: run   Space: jump   Drag: look   Scroll: zoom"
        } else {
            "WASD: move   Shift: run   Space: jump   Drag: look   Scroll: zoom   Tab: builder"
        }),
        TextFont { font_size: FontSize::Px(14.0), ..default() },
        TextColor(Color::srgba(1.0, 1.0, 1.0, 0.75)),
        Node {
            position_type: PositionType::Absolute,
            left: px(12),
            bottom: px(10),
            ..default()
        },
    ));
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
