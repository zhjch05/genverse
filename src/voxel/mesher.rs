//! Turns a [`VoxelGrid`] into chunked triangle meshes.
//!
//! Only faces touching air (or a transparent neighbor) are emitted. Each vertex gets
//! baked ambient occlusion from its three neighbors and a per-voxel color jitter; together
//! with the renderer's SSAO and soft shadows this gives the smooth "HD voxel" look rather
//! than flat, textured blocks.
//!
//! Each face also carries what `voxel_bevel.wesl` needs to round its outside edges:
//! which of its edges turn away from it.
//!
//! A vertex is two words ([`ATTRIBUTE_VOXEL`]), 8 bytes in place of the 72 of Bevy's
//! position, normal, color, UVs and tangent; `voxel_vertex.wesl` rebuilds those. The
//! first word packs the corner's position in the chunk, in voxels (6 bits per axis),
//! which face (3) and corner (2) it is, its light (2), and the face's edge flags (4).
//! The second holds the voxel's sRGB color. Positions are in voxels, so chunk entities
//! are scaled by the voxel size.

use bevy::{
    asset::RenderAssetUsages,
    math::Vec3A,
    mesh::{Indices, MeshVertexAttribute, PrimitiveTopology, VertexFormat},
    prelude::*,
    shape::Aabb3d,
};

use cubewish_rules::voxel::faces::face_visible;

use super::{AIR, Palette, VoxelGrid, VoxelId, noise::hash01};

/// Edge length of a mesh chunk, in voxels.
pub const CHUNK_SIZE: i32 = 32;

/// The packed vertex, see the module docs.
pub const ATTRIBUTE_VOXEL: MeshVertexAttribute =
    MeshVertexAttribute::new("Voxel", 0x7c3e_91a4_5b02_d6f1, VertexFormat::Uint32x2);

struct Face {
    normal: IVec3,
    /// Corners of the unit cube, counter-clockwise when seen from outside.
    corners: [IVec3; 4],
}

const fn v(x: i32, y: i32, z: i32) -> IVec3 {
    IVec3::new(x, y, z)
}

const FACES: [Face; 6] = [
    Face { normal: v(1, 0, 0), corners: [v(1, 0, 0), v(1, 1, 0), v(1, 1, 1), v(1, 0, 1)] },
    Face { normal: v(-1, 0, 0), corners: [v(0, 0, 0), v(0, 0, 1), v(0, 1, 1), v(0, 1, 0)] },
    Face { normal: v(0, 1, 0), corners: [v(0, 1, 0), v(0, 1, 1), v(1, 1, 1), v(1, 1, 0)] },
    Face { normal: v(0, -1, 0), corners: [v(0, 0, 0), v(1, 0, 0), v(1, 0, 1), v(0, 0, 1)] },
    Face { normal: v(0, 0, 1), corners: [v(0, 0, 1), v(1, 0, 1), v(1, 1, 1), v(0, 1, 1)] },
    Face { normal: v(0, 0, -1), corners: [v(0, 0, 0), v(0, 1, 0), v(1, 1, 0), v(1, 0, 0)] },
];

/// One mesh per (chunk, surface) pair, positioned relative to `origin`, in voxels.
pub struct ChunkMesh {
    pub origin: IVec3,
    pub surface: usize,
    pub mesh: Mesh,
}

struct MeshBuilder {
    vertices: Vec<[u32; 2]>,
    indices: Vec<u32>,
    /// Bounds of the corners so far.
    min: IVec3,
    max: IVec3,
}

impl MeshBuilder {
    fn new() -> Self {
        Self { vertices: Vec::new(), indices: Vec::new(), min: IVec3::MAX, max: IVec3::MIN }
    }

    fn build(self) -> Mesh {
        let indices = if self.vertices.len() <= 1 << 16 {
            Indices::U16(self.indices.iter().map(|&i| i as u16).collect())
        } else {
            Indices::U32(self.indices)
        };
        let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD)
            .with_inserted_attribute(ATTRIBUTE_VOXEL, self.vertices)
            .with_inserted_indices(indices);
        // There's no position attribute to measure, so culling gets the bounds from here.
        mesh.final_aabb = Some(Aabb3d { min: Vec3A::from(self.min.as_vec3()), max: Vec3A::from(self.max.as_vec3()) });
        mesh
    }
}

/// Which edges of the face of `id` at `p` are outside edges, where the voxel's side
/// face beyond them shows too: bit 0 for u = 0, 1 for u = 1, 2 for v = 0, 3 for v = 1.
/// Transparent voxels stay sharp, so water and glass meet flat.
fn outside_edges(get: &impl Fn(IVec3) -> VoxelId, palette: &Palette, p: IVec3, id: VoxelId, face: &Face) -> u32 {
    if !palette.is_opaque(id) {
        return 0;
    }
    let u = face.corners[1] - face.corners[0];
    let v = face.corners[3] - face.corners[0];
    [-u, u, -v, v]
        .iter()
        .enumerate()
        .filter(|(_, d)| face_visible(palette, id, get(p + **d)))
        .fold(0, |bits, (i, _)| bits | 1 << i)
}

/// Number of unoccluded neighbors (0..=3) around one face corner.
fn corner_light(
    get: &impl Fn(IVec3) -> VoxelId,
    palette: &Palette,
    p: IVec3,
    face: &Face,
    corner: IVec3,
) -> usize {
    let layer = p + face.normal;
    let mut offsets = [IVec3::ZERO; 2];
    let mut n = 0;
    for axis in 0..3 {
        if face.normal[axis] == 0 {
            offsets[n][axis] = if corner[axis] == 1 { 1 } else { -1 };
            n += 1;
        }
    }
    let solid = |q: IVec3| palette.is_opaque(get(q));
    let side1 = solid(layer + offsets[0]);
    let side2 = solid(layer + offsets[1]);
    if side1 && side2 {
        return 0;
    }
    let corner = solid(layer + offsets[0] + offsets[1]);
    3 - (side1 as usize + side2 as usize + corner as usize)
}

/// Meshes the voxels in `min..max`, at most [`CHUNK_SIZE`] on a side, reading neighbors
/// (which may lie outside the region) through `get`. Returns one mesh per surface,
/// positioned relative to `min`, in voxels.
pub fn mesh_region(get: impl Fn(IVec3) -> VoxelId, palette: &Palette, min: IVec3, max: IVec3) -> Vec<(usize, Mesh)> {
    assert!((max - min).cmple(IVec3::splat(CHUNK_SIZE)).all(), "region too large to pack: {min}..{max}");
    let mut builders: Vec<MeshBuilder> = palette.surfaces.iter().map(|_| MeshBuilder::new()).collect();

    for z in min.z..max.z {
        for y in min.y..max.y {
            for x in min.x..max.x {
                let p = IVec3::new(x, y, z);
                let id = get(p);
                if id == AIR {
                    continue;
                }
                let entry = palette.entry(id);
                let jitter = 1.0 + entry.variation * (hash01(p, id as u32) * 2.0 - 1.0);
                let [r, g, b, _] = Srgba::from(entry.color * jitter).to_u8_array();
                let color = r as u32 | (g as u32) << 8 | (b as u32) << 16;
                let builder = &mut builders[entry.surface];

                for (f, face) in FACES.iter().enumerate() {
                    if !face_visible(palette, id, get(p + face.normal)) {
                        continue;
                    }
                    let light = face.corners.map(|c| corner_light(&get, palette, p, face, c));
                    let edges = outside_edges(&get, palette, p, id, face);
                    let start = builder.vertices.len() as u32;
                    for (c, (corner, l)) in face.corners.iter().zip(light).enumerate() {
                        let local = (p - min + *corner).as_uvec3();
                        builder.min = builder.min.min(local.as_ivec3());
                        builder.max = builder.max.max(local.as_ivec3());
                        let packed = local.x
                            | local.y << 6
                            | local.z << 12
                            | (f as u32) << 18
                            | (c as u32) << 21
                            | (l as u32) << 23
                            | edges << 25;
                        builder.vertices.push([packed, color]);
                    }
                    // Split the quad along the brighter diagonal so AO
                    // gradients don't show a visible crease.
                    let quad = if light[0] + light[2] >= light[1] + light[3] {
                        [0, 1, 2, 0, 2, 3]
                    } else {
                        [1, 2, 3, 1, 3, 0]
                    };
                    builder.indices.extend(quad.map(|i| start + i));
                }
            }
        }
    }

    builders
        .into_iter()
        .enumerate()
        .filter(|(_, b)| !b.indices.is_empty())
        .map(|(surface, b)| (surface, b.build()))
        .collect()
}

/// Meshes a whole grid in [`CHUNK_SIZE`] chunks.
pub fn mesh_grid(grid: &VoxelGrid, palette: &Palette) -> Vec<ChunkMesh> {
    let size = grid.size().as_ivec3();
    let chunk_counts = (size + IVec3::splat(CHUNK_SIZE - 1)) / CHUNK_SIZE;
    let mut out = Vec::new();
    for cz in 0..chunk_counts.z {
        for cy in 0..chunk_counts.y {
            for cx in 0..chunk_counts.x {
                let origin = IVec3::new(cx, cy, cz) * CHUNK_SIZE;
                let end = (origin + IVec3::splat(CHUNK_SIZE)).min(size);
                for (surface, mesh) in mesh_region(|p| grid.get(p), palette, origin, end) {
                    out.push(ChunkMesh { origin, surface, mesh });
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `voxel_vertex.wesl` hard-codes each face's normal and tangent (u) and takes the
    /// tangent's w as 1, which holds while every bitangent `normal × u` points along v.
    #[test]
    fn faces_match_the_vertex_shader() {
        let normals = [IVec3::X, IVec3::NEG_X, IVec3::Y, IVec3::NEG_Y, IVec3::Z, IVec3::NEG_Z];
        let tangents = [IVec3::Y, IVec3::Z, IVec3::Z, IVec3::X, IVec3::X, IVec3::Y];
        for (i, face) in FACES.iter().enumerate() {
            let u = face.corners[1] - face.corners[0];
            let v = face.corners[3] - face.corners[0];
            assert_eq!(face.normal, normals[i]);
            assert_eq!(u, tangents[i]);
            assert_eq!(face.normal.cross(u), v);
        }
    }
}
