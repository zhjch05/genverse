//! The rules of a Cubewish world, shared by the game and (later) a server.
//!
//! Everything that decides what the world *is* lives here, with no rendering: voxel
//! models and how they rasterize, the terrain generator, terrain changes, agent build
//! specs, and the limits they must pass. Two programs that run the same rules on the
//! same inputs get the same voxels, which is what lets a server trust specs instead of
//! voxels and lets saves replay.

pub mod budget;
pub mod build;
pub mod game;
pub mod identity;
pub mod land;
pub mod log;
pub mod logic;
pub mod rate;
pub mod sculpt;
pub mod semantic;
pub mod settings;
#[cfg(not(target_arch = "wasm32"))]
pub mod store;
pub mod terrain;
pub mod undo;
pub mod voxel;

pub use bevy_math::IVec3;
