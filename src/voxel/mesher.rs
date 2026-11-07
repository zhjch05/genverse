//! Turns a [`VoxelGrid`] into chunked triangle meshes.
//!
//! Only faces touching air (or a transparent neighbor) are emitted. Each vertex gets
//! baked ambient occlusion from its three neighbors and a per-voxel color jitter; together
//! with the renderer's SSAO and soft shadows this gives the smooth "HD voxel" look rather
//! than flat, textured blocks.
//!
//! Each face also carries what `voxel_bevel.wesl` needs to round its outside edges: its
//! position across the face (UV 0), which of its edges turn away from it (UV 1), and the
//! direction across it (the tangent).

use bevy::{
    asset::RenderAssetUsages,
    mesh::{Indices, PrimitiveTopology},
    prelude::*,
};

use cubelark_rules::voxel::faces::face_visible;

use super::{AIR, Palette, VoxelGrid, VoxelId, noise::hash01};

/// Edge length of a mesh chunk, in voxels.
pub const CHUNK_SIZE: i32 = 32;

/// Brightness for 0..=3 unoccluded neighbors around a vertex.
const AO_CURVE: [f32; 4] = [0.45, 0.64, 0.82, 1.0];

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

/// One mesh per (chunk, surface) pair, positioned relative to `origin`.
pub struct ChunkMesh {
    pub origin: IVec3,
    pub surface: usize,
    pub mesh: Mesh,
}

#[derive(Default)]
struct MeshBuilder {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    colors: Vec<[f32; 4]>,
    uvs: Vec<[f32; 2]>,
    edges: Vec<[f32; 2]>,
    tangents: Vec<[f32; 4]>,
    indices: Vec<u32>,
}

impl MeshBuilder {
    fn build(self) -> Mesh {
        Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD)
            .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, self.positions)
            .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, self.normals)
            .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, self.colors)
            .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, self.uvs)
            .with_inserted_attribute(Mesh::ATTRIBUTE_UV_1, self.edges)
            .with_inserted_attribute(Mesh::ATTRIBUTE_TANGENT, self.tangents)
            .with_inserted_indices(Indices::U32(self.indices))
    }
}

/// A face corner's position across the face, in the order of [`Face::corners`].
const CORNER_UVS: [[f32; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];

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

/// Meshes the voxels in `min..max`, reading neighbors (which may lie outside the
/// region) through `get`. Returns one mesh per surface, positioned relative to `min`.
pub fn mesh_region(
    get: impl Fn(IVec3) -> VoxelId,
    palette: &Palette,
    voxel_size: f32,
    min: IVec3,
    max: IVec3,
) -> Vec<(usize, Mesh)> {
    let mut builders: Vec<MeshBuilder> =
        palette.surfaces.iter().map(|_| MeshBuilder::default()).collect();

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
                let base = entry.color * jitter;
                let b = &mut builders[entry.surface];

                for face in &FACES {
                    if !face_visible(palette, id, get(p + face.normal)) {
                        continue;
                    }
                    let light = face.corners.map(|c| corner_light(&get, palette, p, face, c));
                    let edges = outside_edges(&get, palette, p, id, face) as f32;
                    let u = (face.corners[1] - face.corners[0]).as_vec3();
                    let v = (face.corners[3] - face.corners[0]).as_vec3();
                    // Bevy's bitangent is `w * cross(normal, tangent)`; make it point along v.
                    let w = face.normal.as_vec3().cross(u).dot(v).signum();
                    let start = b.positions.len() as u32;
                    for ((corner, l), uv) in face.corners.iter().zip(light).zip(CORNER_UVS) {
                        let local = (p - min + *corner).as_vec3() * voxel_size;
                        let shade = AO_CURVE[l];
                        b.positions.push(local.into());
                        b.normals.push(face.normal.as_vec3().into());
                        b.colors.push([base.red * shade, base.green * shade, base.blue * shade, 1.0]);
                        b.uvs.push(uv);
                        b.edges.push([edges, 0.0]);
                        b.tangents.push([u.x, u.y, u.z, w]);
                    }
                    // Split the quad along the brighter diagonal so AO
                    // gradients don't show a visible crease.
                    let quad = if light[0] + light[2] >= light[1] + light[3] {
                        [0, 1, 2, 0, 2, 3]
                    } else {
                        [1, 2, 3, 1, 3, 0]
                    };
                    b.indices.extend(quad.map(|i| start + i));
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
pub fn mesh_grid(grid: &VoxelGrid, palette: &Palette, voxel_size: f32) -> Vec<ChunkMesh> {
    let size = grid.size().as_ivec3();
    let chunk_counts = (size + IVec3::splat(CHUNK_SIZE - 1)) / CHUNK_SIZE;
    let mut out = Vec::new();
    for cz in 0..chunk_counts.z {
        for cy in 0..chunk_counts.y {
            for cx in 0..chunk_counts.x {
                let origin = IVec3::new(cx, cy, cz) * CHUNK_SIZE;
                let end = (origin + IVec3::splat(CHUNK_SIZE)).min(size);
                for (surface, mesh) in mesh_region(|p| grid.get(p), palette, voxel_size, origin, end) {
                    out.push(ChunkMesh { origin, surface, mesh });
                }
            }
        }
    }
    out
}
