//! Super Simple Cubes — client⇄server protocol.
//!
//! Hand-rolled binary codec (`std` only, per the minimal-dependency policy in
//! `specs.md` §4). The client is 100% server-driven: every world event,
//! including sound triggers, is a packet from here.
//!
//! Status: architecture phase; packets land with the server.
