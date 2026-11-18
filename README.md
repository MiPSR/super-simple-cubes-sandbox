# Super Simple Cubes

A from-scratch Minecraft-like voxel game in Rust: a thin, **server-driven**
client on a custom trait-based engine (OpenGL/Vulkan), and a fully
**authoritative server** that runs either embedded in the client (local play)
or as a dedicated headless process.

- `specs.md` — the immutable project specification.
- `agents.md` — contribution instructions for agents/humans.
- `implemented-features.md` — wiki-feature parity tracker.

## Layout

| Crate | Role |
|---|---|
| `crates/ssc-engine` | Rendering engine; the only crate allowed to touch GFX APIs. Backends: OpenGL (default), Vulkan. |
| `crates/ssc-math` | Math primitives shared by everything. |
| `crates/ssc-world` | Blocks, chunks, world data types (shared client/server, no gameplay logic). |
| `crates/ssc-protocol` | Client⇄server network protocol (packets + codec). |
| `crates/ssc-lod` | Distant Horizons-style far-terrain LOD. |
| `crates/ssc-server` | All game logic. Binary: `ssc-server` (dedicated), lib for embedded local use. |
| `crates/ssc-client` | The game client. Binary: `ssc-client`. Renders and sends input — nothing else. |

## Nix (the only supported way to build)

```console
nix run .#ssc-client   # play (local embedded server)
nix run .#ssc-server   # dedicated server

nix build .#ssc-client .#ssc-server
nix develop            # rust toolchain + runtime libs for dev
```

The host needs no Rust toolchain; everything goes through Nix.

## Current state

Pre-alpha: engine architecture phase. See `implemented-features.md` for what
actually works.
