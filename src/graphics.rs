//! Graphics settings: how much of the lighting and post-processing runs, and at what
//! resolution the world is drawn. Each option has its own level, the way desktop games
//! list them; a [`Quality`] preset sets them all at once.
//!
//! The world camera always draws into an image (the "picture"), which a second camera
//! shows over the whole window under the interface. Below 100% render scale the picture
//! is smaller and gets stretched, while text stays sharp. Its logical size is the
//! window's (its scale factor makes up the difference), so cursor rays and screen
//! positions work the same at any scale. The world camera never switches between the
//! window and the picture: that switch, and the two cameras sharing the window, left
//! frames where depth and color buffers disagreed in size, which wgpu rejects.
//!
//! The cost is per pixel: on an M1 Max at Retina resolution, everything on (Ultra) ran
//! about 20 fps, the Low preset about twice that, and Ultra at half the pixels about 60.

use bevy::{
    anti_alias::{fxaa::Fxaa, taa::TemporalAntiAliasing},
    camera::{CameraUpdateSystems, Hdr, ImageRenderTarget, RenderTarget, visibility::RenderLayers},
    core_pipeline::prepass::{DepthPrepass, MotionVectorPrepass, NormalPrepass},
    image::{ImageSampler, ImageSamplerDescriptor},
    light::{CascadeShadowConfigBuilder, DirectionalLightShadowMap, ShadowFilteringMethod},
    pbr::{ScreenSpaceAmbientOcclusion, ScreenSpaceAmbientOcclusionQualityLevel},
    post_process::bloom::Bloom,
    prelude::*,
    render::{
        camera::{MipBias, TemporalJitter},
        render_resource::{Extent3d, TextureFormat},
        view::{Msaa, Tonemapping},
    },
    sprite_render::AlphaMode2d,
    window::PrimaryWindow,
};
use serde::{Deserialize, Serialize};

use crate::player::FollowCamera;

pub struct GraphicsPlugin;

impl Plugin for GraphicsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Graphics>()
            .add_systems(Startup, spawn_screen)
            // Before the cameras work out their sizes, so the world camera sees a resized
            // picture in the same frame.
            .add_systems(PostUpdate, (apply_graphics, fit_render_target).chain().before(CameraUpdateSystems));
    }
}

/// How much detail an effect gets.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub enum Level {
    Off,
    Low,
    Medium,
    High,
    Ultra,
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
impl Level {
    pub const ALL: [Level; 5] = [Level::Off, Level::Low, Level::Medium, Level::High, Level::Ultra];

    pub fn label(self) -> &'static str {
        match self {
            Level::Off => "Off",
            Level::Low => "Low",
            Level::Medium => "Medium",
            Level::High => "High",
            Level::Ultra => "Ultra",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Antialias {
    Off,
    /// Cheap and per frame: softens edges a little.
    Fxaa,
    /// Smooth edges and soft shadows, from the last few frames; needs a prepass.
    Taa,
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
impl Antialias {
    pub const ALL: [Antialias; 3] = [Antialias::Off, Antialias::Fxaa, Antialias::Taa];

    pub fn label(self) -> &'static str {
        match self {
            Antialias::Off => "Off",
            Antialias::Fxaa => "FXAA",
            Antialias::Taa => "TAA",
        }
    }
}

/// Every option, as the settings window lists them. Kept in saves/settings.json on
/// desktop; the web runs the default.
#[derive(Resource, Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Graphics {
    pub antialias: Antialias,
    /// Screen-space ambient occlusion; the meshes' baked corner shading stays when off.
    pub ambient_occlusion: Level,
    pub shadows: Level,
    pub contact_shadows: bool,
    pub bloom: bool,
    /// The share of the window's pixels the world is drawn at, 0.5 to 1.
    pub render_scale: f32,
}

impl Default for Graphics {
    fn default() -> Self {
        Quality::Medium.settings(1.0)
    }
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub const RENDER_SCALES: [f32; 5] = [0.5, 0.67, 0.75, 0.85, 1.0];

/// A starting point for every option but the render scale, which is the player's own.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quality {
    /// No SSAO, FXAA instead of TAA (so no prepass at all), short hard-edged shadows,
    /// no bloom.
    Low,
    /// No SSAO, TAA, two shadow cascades.
    Medium,
    /// Light SSAO, three shadow cascades.
    High,
    /// Everything: full SSAO, four cascades, contact shadows.
    Ultra,
}

impl Quality {
    pub const ALL: [Quality; 4] = [Quality::Low, Quality::Medium, Quality::High, Quality::Ultra];

    pub fn label(self) -> &'static str {
        match self {
            Quality::Low => "Low",
            Quality::Medium => "Medium",
            Quality::High => "High",
            Quality::Ultra => "Ultra",
        }
    }

    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|q| q.label().eq_ignore_ascii_case(key))
    }

    pub fn settings(self, render_scale: f32) -> Graphics {
        let (antialias, ambient_occlusion, shadows, contact_shadows, bloom) = match self {
            Quality::Low => (Antialias::Fxaa, Level::Off, Level::Low, false, false),
            Quality::Medium => (Antialias::Taa, Level::Off, Level::Medium, false, true),
            Quality::High => (Antialias::Taa, Level::Low, Level::High, false, true),
            Quality::Ultra => (Antialias::Taa, Level::High, Level::Ultra, true, true),
        };
        Graphics { antialias, ambient_occlusion, shadows, contact_shadows, bloom, render_scale }
    }
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
impl Graphics {
    /// The preset these settings match, if any ("Custom" otherwise).
    pub fn preset(&self) -> Option<Quality> {
        Quality::ALL.into_iter().find(|q| q.settings(self.render_scale) == *self)
    }

    fn ssao(&self) -> Option<ScreenSpaceAmbientOcclusionQualityLevel> {
        match self.ambient_occlusion {
            Level::Off => None,
            Level::Low => Some(ScreenSpaceAmbientOcclusionQualityLevel::Low),
            Level::Medium => Some(ScreenSpaceAmbientOcclusionQualityLevel::Medium),
            Level::High => Some(ScreenSpaceAmbientOcclusionQualityLevel::High),
            Level::Ultra => Some(ScreenSpaceAmbientOcclusionQualityLevel::Ultra),
        }
    }

    /// Cascades, how far shadows reach in meters, and each cascade's map size.
    fn shadow_maps(&self) -> Option<(usize, f32, usize)> {
        match self.shadows {
            Level::Off => None,
            Level::Low => Some((2, 40.0, 1024)),
            Level::Medium => Some((2, 55.0, 2048)),
            Level::High => Some((3, 70.0, 2048)),
            Level::Ultra => Some((4, 70.0, 2048)),
        }
    }

    fn scale(&self) -> f32 {
        self.render_scale.clamp(0.25, 1.0)
    }
}

/// The camera that draws the picture of the world and the interface over it: the only
/// one drawing to the window.
#[derive(Component)]
pub struct ScreenCamera;

/// The rectangle the picture of the world is shown on. Opaque, whatever the picture's
/// alpha says, so nothing from the last frame shows through.
#[derive(Component)]
struct WorldPicture;

/// Drawn by the screen camera only.
const SCREEN_LAYER: usize = 1;

pub fn spawn_screen(mut commands: Commands, mut meshes: ResMut<Assets<Mesh>>, mut materials: ResMut<Assets<ColorMaterial>>) {
    commands.spawn((
        Name::new("Screen camera"),
        ScreenCamera,
        Camera2d,
        // After the world camera, so its picture is ready.
        Camera { order: 1, ..default() },
        // The interface blends in HDR, as it did when it drew on the world camera; the
        // picture is already tonemapped.
        Hdr,
        Tonemapping::None,
        Msaa::Off,
        RenderLayers::layer(SCREEN_LAYER),
    ));
    commands.spawn((
        Name::new("World picture"),
        WorldPicture,
        Mesh2d(meshes.add(Rectangle::new(1.0, 1.0))),
        MeshMaterial2d(materials.add(ColorMaterial { alpha_mode: AlphaMode2d::Opaque, ..default() })),
        RenderLayers::layer(SCREEN_LAYER),
    ));
}

/// Sets up the world camera and the sun for the current settings, at the start and
/// whenever they change.
fn apply_graphics(
    graphics: Res<Graphics>,
    mut commands: Commands,
    mut shadow_map: ResMut<DirectionalLightShadowMap>,
    cameras: Query<Entity, With<FollowCamera>>,
    mut suns: Query<(Entity, &mut DirectionalLight)>,
    mut started: Local<bool>,
) {
    // The cameras appear at Startup; the first pass waits for them.
    if (!graphics.is_changed() && *started) || cameras.is_empty() {
        return;
    }
    *started = true;
    let g = *graphics;
    for camera in &cameras {
        let mut camera = commands.entity(camera);
        // Their prepasses and jitter stay behind when TAA or SSAO is removed, so they go too.
        camera.remove::<(
            TemporalAntiAliasing,
            TemporalJitter,
            MipBias,
            Fxaa,
            ScreenSpaceAmbientOcclusion,
            DepthPrepass,
            NormalPrepass,
            MotionVectorPrepass,
            Bloom,
        )>();
        match g.antialias {
            Antialias::Off => {}
            Antialias::Fxaa => {
                camera.insert(Fxaa::default());
            }
            Antialias::Taa => {
                camera.insert(TemporalAntiAliasing::default());
            }
        }
        // Temporal shadow filtering only settles with TAA; hard edges are cheapest.
        camera.insert(match (g.antialias, g.shadows) {
            (Antialias::Taa, _) => ShadowFilteringMethod::Temporal,
            (_, Level::Low) => ShadowFilteringMethod::Hardware2x2,
            _ => ShadowFilteringMethod::Gaussian,
        });
        if g.bloom {
            camera.insert(Bloom::NATURAL);
        }
        // SSAO isn't supported on WebGPU yet; baked voxel AO still applies there.
        if let Some(quality_level) = g.ssao()
            && cfg!(not(target_arch = "wasm32"))
        {
            camera.insert(ScreenSpaceAmbientOcclusion { quality_level, ..default() });
        }
    }
    let maps = g.shadow_maps();
    if let Some((_, _, size)) = maps
        && shadow_map.size != size
    {
        shadow_map.size = size;
    }
    for (sun, mut light) in &mut suns {
        light.shadow_maps_enabled = maps.is_some();
        light.contact_shadows_enabled = g.contact_shadows && maps.is_some();
        if let Some((num_cascades, maximum_distance, _)) = maps {
            let cascades =
                CascadeShadowConfigBuilder { num_cascades, first_cascade_far_bound: 8.0, maximum_distance, ..default() }.build();
            commands.entity(sun).insert(cascades);
        }
    }
}

/// Keeps the picture the world camera draws into sized for the window and the render
/// scale: resized in place, so the screen never waits on a new one.
fn fit_render_target(
    graphics: Res<Graphics>,
    mut images: ResMut<Assets<Image>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    mut world_cameras: Query<&mut RenderTarget, With<FollowCamera>>,
    mut pictures: Query<(&MeshMaterial2d<ColorMaterial>, &mut Transform), With<WorldPicture>>,
) {
    let (Ok(window), Ok(mut target), Ok((material, mut transform))) =
        (windows.single(), world_cameras.single_mut(), pictures.single_mut())
    else {
        return;
    };
    let physical = window.physical_size();
    if physical.x == 0 || physical.y == 0 {
        return;
    }
    let scale = graphics.scale();
    let size = (physical.as_vec2() * scale).round().as_uvec2().max(UVec2::ONE);
    // The picture's logical size matches the window's, so rays from the cursor line up.
    let scale_factor = window.scale_factor() * size.x as f32 / physical.x as f32;
    // At full size, a straight copy; smaller, smoothed as it stretches.
    let sampler = if size == physical { ImageSamplerDescriptor::nearest() } else { ImageSamplerDescriptor::linear() };
    match &*target {
        RenderTarget::Image(picture) => {
            let (handle, current_factor) = (picture.handle.clone(), picture.scale_factor);
            if let Some(image) = images.get(&handle)
                && (image.size() != size || !matches!(&image.sampler, ImageSampler::Descriptor(d) if *d == sampler))
                && let Some(mut image) = images.get_mut(&handle)
            {
                image.resize(Extent3d { width: size.x, height: size.y, depth_or_array_layers: 1 });
                image.sampler = ImageSampler::Descriptor(sampler);
                // The material's bind group still holds the old texture, so the screen would
                // keep showing the last frame drawn before the resize; writing to it rebuilds that.
                if let Some(mut material) = materials.get_mut(&material.0) {
                    material.texture = Some(handle.clone());
                }
            }
            if current_factor != scale_factor {
                *target = RenderTarget::Image(ImageRenderTarget { handle, scale_factor });
            }
        }
        _ => {
            let mut image = Image::new_target_texture(size.x, size.y, TextureFormat::Bgra8UnormSrgb, None);
            image.sampler = ImageSampler::Descriptor(sampler);
            let handle = images.add(image);
            *target = RenderTarget::Image(ImageRenderTarget { handle: handle.clone(), scale_factor });
            if let Some(mut material) = materials.get_mut(&material.0) {
                material.texture = Some(handle);
            }
        }
    }
    let size = window.size().extend(1.0);
    if transform.scale != size {
        transform.scale = size;
    }
}
