use bevy_math::{IVec3, UVec3};

/// Index into a [`Palette`](super::palette::Palette). `0` is reserved for empty space.
pub type VoxelId = u8;

pub const AIR: VoxelId = 0;

/// Dense 3D voxel storage addressed by integer `(x, y, z)` coordinates, with `+Y` up.
///
/// Cells are laid out x-fastest, then y, then z.
#[derive(Clone, Debug)]
pub struct VoxelGrid {
    size: UVec3,
    cells: Vec<VoxelId>,
}

impl VoxelGrid {
    pub fn new(size: UVec3) -> Self {
        Self {
            size,
            cells: vec![AIR; (size.x * size.y * size.z) as usize],
        }
    }

    pub fn size(&self) -> UVec3 {
        self.size
    }

    /// Every cell, in the grid's layout.
    pub fn cells(&self) -> &[VoxelId] {
        &self.cells
    }

    pub fn contains(&self, p: IVec3) -> bool {
        p.cmpge(IVec3::ZERO).all() && p.cmplt(self.size.as_ivec3()).all()
    }

    fn index(&self, p: IVec3) -> usize {
        (p.x as u32 + self.size.x * (p.y as u32 + self.size.y * p.z as u32)) as usize
    }

    /// Returns [`AIR`] for coordinates outside the grid.
    pub fn get(&self, p: IVec3) -> VoxelId {
        if self.contains(p) {
            self.cells[self.index(p)]
        } else {
            AIR
        }
    }

    /// Writes outside the grid are ignored.
    pub fn set(&mut self, p: IVec3, id: VoxelId) {
        if self.contains(p) {
            let i = self.index(p);
            self.cells[i] = id;
        }
    }
}
