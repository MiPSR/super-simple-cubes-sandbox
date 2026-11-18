//! Super Simple Cubes — rendering engine.
//!
//! This crate is the **only** component of the project allowed to talk to
//! windowing/graphics APIs (`specs.md` §2, §3). The game client drives the
//! engine through the traits defined here; backends are selected at build
//! time via Cargo features:
//!
//! - `opengl` (default)
//! - `vulkan`
//!
//! Visual baseline per spec: textured cubes, no lighting, no shader effects.
//! Priorities: correct culling → proper rendering → high framerate.
//!
//! Status: architecture phase. Traits and backends land in the next commits.

#![forbid(unsafe_code)] // backend modules will locally relax this where needed
