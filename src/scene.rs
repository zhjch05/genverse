//! Golden-hour presentation: physical sky, soft sun shadows, SSAO, TAA, bloom,
//! and an orbit camera around the sample model.

use std::f32::consts::PI;

use bevy::{
    anti_alias::taa::TemporalAntiAliasing,
    camera::Exposure,
    input::mouse::{AccumulatedMouseMotion, AccumulatedMouseScroll, MouseScrollUnit},
    light::{
        Atmosphere, AtmosphereEnvironmentMapLight, CascadeShadowConfigBuilder,
        ShadowFilteringMethod, atmosphere::ScatteringMedium, light_consts::lux,
    },
    pbr::{AtmosphereSettings, ScreenSpaceAmbientOcclusion},
    post_process::bloom::Bloom,
    prelude::*,
    render::view::{
        Msaa, Tonemapping,
        screenshot::{Screenshot, save_to_disk},
    },
};

use crate::voxel::{VoxelModel, VoxelModelSource};

pub struct ScenePlugin;

impl Plugin for ScenePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, setup)
            .add_systems(Update, (orbit_camera, draw_axes));
        // `CUBELARK_SHOT=out.png [CUBELARK_YAW=0.5] cargo run` renders one still and exits.
        if let Ok(path) = std::env::var("CUBELARK_SHOT") {
            app.insert_resource(StillShot { path, frame: 0 })
                .add_systems(Update, take_still_shot);
        }
    }
}

#[derive(Component)]
struct OrbitCamera {
    focus: Vec3,
    yaw: f32,
    pitch: f32,
    distance: f32,
    auto_rotate: bool,
}

#[derive(Resource)]
struct ShowAxes(bool);

#[derive(Resource)]
struct StillShot {
    path: String,
    frame: u32,
}

fn setup(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mut media: ResMut<Assets<ScatteringMedium>>,
) {
    commands.spawn((
        Name::new("Sakura island"),
        VoxelModelSource(asset_server.load("models/sakura_island.vox.ron")),
    ));

    // Hang the island a few kilometers up so the planet below dissolves into haze.
    let mut atmosphere = Atmosphere::earth(media.add(ScatteringMedium::default()));
    // A bright, cloud-like ground reads as a sea of clouds far below.
    atmosphere.ground_albedo = Vec3::new(0.75, 0.78, 0.85);
    let altitude = 3_000.0;
    commands.spawn((
        Name::new("Sky"),
        Transform::from_xyz(0.0, -(atmosphere.inner_radius + altitude), 0.0),
        atmosphere,
    ));

    // Low, warm side light: long shadows and rim-lit blossoms.
    commands.spawn((
        Name::new("Sun"),
        DirectionalLight {
            illuminance: lux::RAW_SUNLIGHT,
            shadow_maps_enabled: true,
            contact_shadows_enabled: true,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::YXZ, PI * 0.42, -0.3, 0.0)),
        CascadeShadowConfigBuilder {
            num_cascades: 3,
            first_cascade_far_bound: 10.0,
            maximum_distance: 50.0,
            ..default()
        }
        .build(),
    ));

    let mut camera = commands.spawn((
        Name::new("Camera"),
        Camera3d::default(),
        Msaa::Off,
        TemporalAntiAliasing::default(),
        ShadowFilteringMethod::Temporal,
        AtmosphereSettings::default(),
        AtmosphereEnvironmentMapLight::default(),
        Exposure { ev100: 13.0 },
        Tonemapping::TonyMcMapface,
        Bloom::NATURAL,
        OrbitCamera {
            auto_rotate: std::env::var("CUBELARK_SHOT").is_err(),
            ..OrbitCamera::from_env()
        },
    ));
    // SSAO isn't supported on WebGPU yet; baked voxel AO still applies there.
    if cfg!(not(target_arch = "wasm32")) {
        camera.insert(ScreenSpaceAmbientOcclusion::default());
    }

    commands.insert_resource(ShowAxes(false));
    commands.spawn((
        Text::new("Drag: orbit   Scroll: zoom   R: auto-rotate   G: axes"),
        TextFont { font_size: FontSize::Px(14.0), ..default() },
        TextColor(Color::srgba(1.0, 1.0, 1.0, 0.7)),
        Node {
            position_type: PositionType::Absolute,
            left: px(12),
            bottom: px(10),
            ..default()
        },
    ));
}

impl OrbitCamera {
    fn from_env() -> Self {
        let env = |k: &str, d: f32| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        Self {
            focus: Vec3::new(0.0, 0.8, 0.0),
            yaw: env("CUBELARK_YAW", -0.6),
            pitch: env("CUBELARK_PITCH", 0.32),
            distance: env("CUBELARK_DIST", 17.0),
            auto_rotate: true,
        }
    }
}

fn take_still_shot(mut commands: Commands, mut shot: ResMut<StillShot>, mut exit: MessageWriter<AppExit>) {
    shot.frame += 1;
    // Give assets time to load and TAA/auto-exposure-free history time to settle.
    if shot.frame == 300 {
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(shot.path.clone()));
    }
    if shot.frame == 330 {
        exit.write(AppExit::Success);
    }
}

fn orbit_camera(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
    motion: Res<AccumulatedMouseMotion>,
    scroll: Res<AccumulatedMouseScroll>,
    mut cameras: Query<(&mut OrbitCamera, &mut Transform)>,
) {
    for (mut orbit, mut transform) in &mut cameras {
        if keys.just_pressed(KeyCode::KeyR) {
            orbit.auto_rotate = !orbit.auto_rotate;
        }
        if buttons.pressed(MouseButton::Left) {
            orbit.yaw -= motion.delta.x * 0.005;
            orbit.pitch = (orbit.pitch + motion.delta.y * 0.005).clamp(-1.2, 1.45);
            orbit.auto_rotate = false;
        } else if orbit.auto_rotate {
            orbit.yaw += time.delta_secs() * 0.06;
        }
        let lines = match scroll.unit {
            MouseScrollUnit::Line => scroll.delta.y,
            MouseScrollUnit::Pixel => scroll.delta.y / 40.0,
        };
        orbit.distance = (orbit.distance * (1.0 - lines * 0.08)).clamp(3.0, 80.0);

        let rotation = Quat::from_euler(EulerRot::YXZ, orbit.yaw, -orbit.pitch, 0.0);
        transform.translation = orbit.focus + rotation * Vec3::Z * orbit.distance;
        transform.look_at(orbit.focus, Vec3::Y);
    }
}

/// X (red), Y (green), Z (blue) axes at each model's origin and voxel-space origin.
fn draw_axes(
    keys: Res<ButtonInput<KeyCode>>,
    mut show: ResMut<ShowAxes>,
    mut gizmos: Gizmos,
    models: Query<(&GlobalTransform, &VoxelModel)>,
) {
    if keys.just_pressed(KeyCode::KeyG) {
        show.0 = !show.0;
    }
    if !show.0 {
        return;
    }
    for (transform, model) in &models {
        gizmos.axes(*transform, 2.0);
        let corner = transform.transform_point(-model.pivot * model.voxel_size);
        gizmos.axes(Transform::from_translation(corner), 1.0);
    }
}
