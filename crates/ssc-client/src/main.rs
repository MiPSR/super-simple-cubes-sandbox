//! `ssc-client` — the game client.
//!
//! Per `specs.md` §2 this process contains **zero game logic**: it renders
//! what the server sends and forwards player input. Even sound triggers come
//! from the server. All rendering goes through `ssc-engine` — never a raw
//! GFX API. For local play it embeds the `ssc-server` library in-process.

fn main() {
    let version = env!("CARGO_PKG_VERSION");
    let backend = if cfg!(feature = "vulkan") {
        "vulkan"
    } else {
        "opengl"
    };
    println!("ssc-client {version} (engine backend: {backend}) — Super Simple Cubes");
    println!("Renderer ships in the upcoming commits; see implemented-features.md");
}
