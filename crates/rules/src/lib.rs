//! The rules of a Cubelark world, shared by the game and (later) a server.
//!
//! Everything that decides what the world *is* lives here, with no rendering: voxel
//! models and how they rasterize, the terrain generator, terrain changes, agent build
//! specs, and the limits they must pass. Two programs that run the same rules on the
//! same inputs get the same voxels, which is what lets a server trust specs instead of
//! voxels and lets saves replay.

pub mod budget;
pub mod build;
pub mod identity;
pub mod land;
pub mod log;
pub mod rate;
pub mod sculpt;
pub mod settings;
pub mod terrain;
pub mod voxel;
