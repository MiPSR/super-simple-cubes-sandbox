//! Super Simple Cubes — client⇄server protocol.
//!
//! Hand-rolled binary codec (`std` only, per the minimal-dependency policy,
//! `specs.md` §4). The client is 100% server-driven: every world event —
//! blocks, columns, LOD data, player state, and sound triggers — is a packet
//! from here (`specs.md` §2).

pub mod codec;
pub mod packets;
pub mod wire;

/// Bumped on any wire-incompatible change.
pub const PROTOCOL_VERSION: u32 = 8;
