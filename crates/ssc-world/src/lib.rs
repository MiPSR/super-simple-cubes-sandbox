//! Super Simple Cubes — shared world data types.
//!
//! Block registry, chunk format, positions. Pure data used by both the
//! server (which owns the logic) and the client (which only renders it).
//! **No gameplay logic may live here** (`specs.md` §2).
//!
//! Status: architecture phase; chunk format lands with the server.
