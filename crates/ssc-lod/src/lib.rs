//! Super Simple Cubes — LOD system (Distant Horizons-style).
//!
//! Far terrain is rendered through this crate from day one (`specs.md` §3).
//! The client builds LOD data from the chunks the server sends; it is
//! presentation only, never gameplay logic.
//!
//! Status: architecture phase; lands with the client renderer.
