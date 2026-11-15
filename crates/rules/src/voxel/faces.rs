//! Which voxel faces are visible: the rule the mesher draws by, and what limits count.

use bevy_math::IVec3;

use super::{AIR, Palette, VoxelGrid, VoxelId};

/// Whether the face of `id` toward `neighbor` is drawn.
pub fn face_visible(palette: &Palette, id: VoxelId, neighbor: VoxelId) -> bool {
    if neighbor == AIR {
        return true;
    }
    if palette.is_opaque(neighbor) {
        return false;
    }
    // Neighbor is transparent: opaque voxels show through it, and different
    // transparent kinds still need a boundary face.
    palette.is_opaque(id) || neighbor != id
}

const NORMALS: [IVec3; 6] = [
    IVec3::X,
    IVec3::NEG_X,
    IVec3::Y,
    IVec3::NEG_Y,
    IVec3::Z,
    IVec3::NEG_Z,
];

/// Visible faces in a grid, with everything outside it counting as air: the real
/// rendering cost of a model.
pub fn count_visible(grid: &VoxelGrid, palette: &Palette) -> usize {
    let size = grid.size().as_ivec3();
    let mut faces = 0;
    for z in 0..size.z {
        for y in 0..size.y {
            for x in 0..size.x {
                let p = IVec3::new(x, y, z);
                let id = grid.get(p);
                if id != AIR {
                    faces += NORMALS
                        .iter()
                        .filter(|n| face_visible(palette, id, grid.get(p + **n)))
                        .count();
                }
            }
        }
    }
    faces
}
