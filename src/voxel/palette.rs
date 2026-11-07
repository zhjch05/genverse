use bevy::prelude::*;

use super::grid::{AIR, VoxelId};

/// How light interacts with a group of voxels. Every surface becomes one
/// [`StandardMaterial`]; voxel colors come from vertex colors.
#[derive(Clone, Debug)]
pub struct Surface {
    pub name: String,
    pub roughness: f32,
    pub metallic: f32,
    pub reflectance: f32,
    /// Emitted light in linear color, already scaled by strength.
    pub emissive: LinearRgba,
    /// Values below `1.0` render alpha-blended (water, glass).
    pub alpha: f32,
}

impl Surface {
    pub fn is_transparent(&self) -> bool {
        self.alpha < 1.0
    }

    pub fn to_material(&self) -> StandardMaterial {
        StandardMaterial {
            base_color: Color::srgba(1.0, 1.0, 1.0, self.alpha),
            perceptual_roughness: self.roughness,
            metallic: self.metallic,
            reflectance: self.reflectance,
            emissive: self.emissive,
            alpha_mode: if self.is_transparent() {
                AlphaMode::Blend
            } else {
                AlphaMode::Opaque
            },
            ..default()
        }
    }
}

#[derive(Clone, Debug)]
pub struct PaletteEntry {
    pub name: String,
    pub color: LinearRgba,
    /// Index into [`Palette::surfaces`].
    pub surface: usize,
    /// Per-voxel brightness jitter, e.g. `0.06` for ±6%.
    pub variation: f32,
}

/// Up to 255 voxel kinds. Entry `0` is [`AIR`] and is never rendered.
#[derive(Clone, Debug)]
pub struct Palette {
    pub surfaces: Vec<Surface>,
    pub entries: Vec<PaletteEntry>,
}

impl Palette {
    pub fn entry(&self, id: VoxelId) -> &PaletteEntry {
        &self.entries[id as usize]
    }

    pub fn surface_of(&self, id: VoxelId) -> &Surface {
        &self.surfaces[self.entry(id).surface]
    }

    /// Solid voxels that block light: used for face culling and baked AO.
    pub fn is_opaque(&self, id: VoxelId) -> bool {
        id != AIR && !self.surface_of(id).is_transparent()
    }

    pub fn find(&self, name: &str) -> Option<VoxelId> {
        if name == "air" {
            return Some(AIR);
        }
        self.entries
            .iter()
            .position(|e| e.name == name)
            .map(|i| i as VoxelId)
    }
}
