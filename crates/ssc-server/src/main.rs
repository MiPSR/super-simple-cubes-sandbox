//! `ssc-server` — dedicated (headless) server entry point.
//!
//! The game logic itself lives in the library part of this crate; this binary
//! only wires it to a network transport and runs it forever.

fn main() {
    let version = env!("CARGO_PKG_VERSION");
    println!("ssc-server {version} (dedicated) — Super Simple Cubes");
    println!("Game logic ships in the upcoming commits; see implemented-features.md");
}
