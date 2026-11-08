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

/// The region being meshed and a one-voxel border, read once, so every neighbor the
/// mesher looks at is an offset into one array, with opacity from a table rather than
/// the palette.
struct Cells {
    cells: Vec<VoxelId>,
    stride: IVec3,
    opaque: [bool; 256],
}

impl Cells {
    fn new(get: impl Fn(IVec3) -> VoxelId, palette: &Palette, min: IVec3, max: IVec3) -> Self {
        let dims = max - min + 2;
        let mut cells = Vec::with_capacity((dims.x * dims.y * dims.z) as usize);
        for z in -1..=max.z - min.z {
            for y in -1..=max.y - min.y {
                for x in -1..=max.x - min.x {
                    cells.push(get(min + IVec3::new(x, y, z)));
                }
            }
        }
        let mut opaque = [false; 256];
        for (id, o) in opaque.iter_mut().enumerate().take(palette.entries.len()) {
            *o = palette.is_opaque(id as VoxelId);
        }
        Self { cells, stride: IVec3::new(1, dims.x, dims.x * dims.y), opaque }
    }

    /// The index of `local` (relative to the region's corner, `-1..=size`).
    fn index(&self, local: IVec3) -> usize {
        (local + 1).dot(self.stride) as usize
    }

    /// How far apart two cells `d` apart are in the array.
    fn delta(&self, d: IVec3) -> isize {
        d.dot(self.stride) as isize
    }

    fn at(&self, i: usize, delta: isize) -> VoxelId {
        self.cells[(i as isize + delta) as usize]
    }

    /// `cubewish_rules::voxel::faces::face_visible`, from the table.
    fn visible(&self, id: VoxelId, neighbor: VoxelId) -> bool {
        neighbor == AIR || (!self.opaque[neighbor as usize] && (self.opaque[id as usize] || neighbor != id))
    }
}

/// A face's neighbors as array offsets, worked out once per region.
struct FaceOffsets {
    normal: isize,
    /// Per corner: the two side neighbors and the diagonal one, in the layer the face
    /// looks into, for its ambient occlusion.
    light: [[isize; 3]; 4],
    /// The neighbors across each edge: u = 0, u = 1, v = 0, v = 1.
    edges: [isize; 4],
}

impl FaceOffsets {
    fn new(cells: &Cells, face: &Face) -> Self {
        let u = face.corners[1] - face.corners[0];
        let v = face.corners[3] - face.corners[0];
        let light = face.corners.map(|corner| {
            let mut offsets = [IVec3::ZERO; 2];
            let mut n = 0;
            for axis in 0..3 {
                if face.normal[axis] == 0 {
                    offsets[n][axis] = if corner[axis] == 1 { 1 } else { -1 };
                    n += 1;
                }
            }
            let layer = face.normal;
            [layer + offsets[0], layer + offsets[1], layer + offsets[0] + offsets[1]].map(|d| cells.delta(d))
        });
        Self { normal: cells.delta(face.normal), light, edges: [-u, u, -v, v].map(|d| cells.delta(d)) }
    }
}

/// Which edges of the face of `id` at cell `i` are outside edges, where the voxel's side
/// face beyond them shows too: bit 0 for u = 0, 1 for u = 1, 2 for v = 0, 3 for v = 1.
/// Transparent voxels stay sharp, so water and glass meet flat.
fn outside_edges(cells: &Cells, i: usize, id: VoxelId, offsets: &FaceOffsets) -> u32 {
    if !cells.opaque[id as usize] {
        return 0;
    }
    offsets.edges.iter().enumerate().filter(|(_, d)| cells.visible(id, cells.at(i, **d))).fold(0, |bits, (b, _)| bits | 1 << b)
}

/// Number of unoccluded neighbors (0..=3) around one face corner.
fn corner_light(cells: &Cells, i: usize, [side1, side2, corner]: [isize; 3]) -> usize {
    let solid = |d: isize| cells.opaque[cells.at(i, d) as usize];
    let (side1, side2) = (solid(side1), solid(side2));
    if side1 && side2 {
        return 0;
    }
    3 - (side1 as usize + side2 as usize + solid(corner) as usize)
}

/// Meshes the voxels in `min..max`, at most [`CHUNK_SIZE`] on a side, reading neighbors
/// (which may lie outside the region) through `get`. Returns one mesh per surface,
/// positioned relative to `min`, in voxels.
pub fn mesh_region(get: impl Fn(IVec3) -> VoxelId, palette: &Palette, min: IVec3, max: IVec3) -> Vec<(usize, Mesh)> {
    let size = max - min;
    assert!(size.cmple(IVec3::splat(CHUNK_SIZE)).all(), "region too large to pack: {min}..{max}");
    let mut builders: Vec<MeshBuilder> = palette.surfaces.iter().map(|_| MeshBuilder::new()).collect();
    let cells = Cells::new(get, palette, min, max);
    let offsets = FACES.each_ref().map(|face| FaceOffsets::new(&cells, face));

    for z in 0..size.z {
        for y in 0..size.y {
            let row = cells.index(IVec3::new(0, y, z));
            for x in 0..size.x {
                let i = row + x as usize;
                let id = cells.cells[i];
                if id == AIR {
                    continue;
                }
                let local = IVec3::new(x, y, z);
                let entry = palette.entry(id);
                let builder = &mut builders[entry.surface];
                // Only for voxels with a face showing: most are buried.
                let mut color = None;

                for (f, (face, offsets)) in FACES.iter().zip(&offsets).enumerate() {
                    if !cells.visible(id, cells.at(i, offsets.normal)) {
                        continue;
                    }
                    let color = *color.get_or_insert_with(|| {
                        let jitter = 1.0 + entry.variation * (hash01(min + local, id as u32) * 2.0 - 1.0);
                        let [r, g, b, _] = Srgba::from(entry.color * jitter).to_u8_array();
                        r as u32 | (g as u32) << 8 | (b as u32) << 16
                    });
                    let light = offsets.light.map(|l| corner_light(&cells, i, l));
                    let edges = outside_edges(&cells, i, id, offsets);
                    let start = builder.vertices.len() as u32;
                    for (c, (corner, l)) in face.corners.iter().zip(light).enumerate() {
                        let corner = (local + *corner).as_uvec3();
                        builder.min = builder.min.min(corner.as_ivec3());
                        builder.max = builder.max.max(corner.as_ivec3());
                        let packed = corner.x
                            | corner.y << 6
                            | corner.z << 12
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
