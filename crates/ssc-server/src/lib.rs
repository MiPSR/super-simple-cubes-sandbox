//! Super Simple Cubes — the authoritative game server (library).
//!
//! **All** game logic lives here (`specs.md` §2): world generation, physics,
//! block updates, mobs, crafting, inventory, and sound triggers. The same
//! library is used by the dedicated binary (`src/main.rs`, over the network)
//! and embedded in the client process for local play.
//!
//! Status: architecture phase; world gen and the tick loop land next.
