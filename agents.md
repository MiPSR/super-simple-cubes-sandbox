# Agents

Instructions for any AI agent or human contributor working on this repository.

1. **Read `specs.md` first.** It is the product source of truth. If code and
   specs disagree, fix the code. If the spec itself seems wrong or blocking,
   ask the owner; update the spec only with explicit owner direction.
2. **Dependency policy**: never add a dependency without owner approval
   (audio / video / gamepad / anything exotic — ask first, with a comparison of
   the popular options). Prefer `std`-only implementations. Every dependency we
   do use must be ultra-popular and well-maintained on GitHub.
3. **The client has no game logic.** Anything gameplay-related belongs in the
   server: world state, physics, block updates, mob AI, crafting, and sound
   triggers. If you find yourself writing gameplay in the client, stop and
   move it to the server.
4. **The client never touches graphics APIs directly.** All rendering goes
   through `ssc-engine` traits. Backends (OpenGL, Vulkan) live only inside the
   engine crate and are selected with Cargo features.
5. **Culling outranks visuals.** Priority order: correct culling → proper
   rendering → high framerate → visual effects. The baseline look is textured
   cubes; no lighting/shaders before the baseline is fast and correct.
6. **LOD is core, not optional.** Far terrain goes through `ssc-lod`
   (Distant Horizons-style) from the start. Do not bolt it on later.
7. **Wiki parity**: implement gameplay per <https://minecraft.wiki/>, Bedrock
   semantics. After each implemented feature, add a line to
   `implemented-features.md` with status:
   - `1:1` — matches wiki behavior;
   - `partial` — simplified/incomplete, reason given;
   - `dev` — intentionally different, reason + reference to `specs.md`.
8. **Build with Nix only.** The host has no Rust toolchain. Use
   `nix build .#<pkg>`, `nix develop`, `nix run .#<app>`. Never assume cargo
   exists outside `nix develop`.
9. **Don't compile constantly.** This project is huge; a build can take a long
   time. Write a large, coherent chunk first, then build once and fix all
   errors in batch.
10. **Commit and push after every chunk.** Small, clean, honest commit
    messages. `git add` the files you touched, commit, push.
11. **Ask focused questions.** When you hit a genuine technical decision for
    the owner, ask one clear question with options (recommended option first).
    Do not block on anything you can decide within `specs.md`.
12. **Mods are prohibited** — including mod-loader-like hooks. Features are
    first-party code with integrated UI.
13. **Sparkle-Morpher comes later.** Keep the engine's mesh/model abstractions
    generic, but do not start on it before core gameplay works.
14. Keep the Rust idiomatic: engine abstraction through traits, `unsafe`
    confined to backend glue where unavoidable, no `unwrap()` in steady-state
    runtime paths.

## Repository map

| Crate | Role |
|---|---|
| `crates/ssc-engine` | Rendering engine; the only crate touching GFX APIs. OpenGL (default) + Vulkan backends. |
| `crates/ssc-math` | Math primitives shared by everything. |
| `crates/ssc-world` | Blocks, chunks, world data types (shared, no gameplay logic). |
| `crates/ssc-protocol` | Client⇄server protocol: packets and codec (shared, no logic). |
| `crates/ssc-lod` | Distant Horizons-style LOD data and meshing. |
| `crates/ssc-server` | All game logic. Bin `ssc-server` (dedicated), lib for embedded/local use. |
| `crates/ssc-client` | The game client. Bin `ssc-client`. Renders + sends input only. |
