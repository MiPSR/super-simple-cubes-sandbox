//! Super Simple Cubes — the authoritative game server (library).
//!
//! **All** game logic lives here (`specs.md` §2): world generation, physics,
//! block updates, interaction raycasts, and sound triggers. The same library
//! is used by the dedicated binary (`src/main.rs`, over the network) and
//! embedded in the client process for local play.
//!
//! Reference for game behavior: <https://minecraft.wiki/> (Bedrock
//! semantics). See `implemented-features.md` for parity status.

mod block_ticks;
pub mod game;
pub mod mobs;
pub mod persistence;
pub mod physics;
pub mod raycast;
pub mod survival;
pub mod worldgen;

pub use game::spawn_local;
pub use game::{GameMode, Server, ServerConfig, HOTBAR};
