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

Pre-alpha engine milestone: working client+server, both buildable and
runnable through Nix.

- Local play: `nix run .#ssc-client` (spawns an embedded server thread).
- Dedicated play: `nix run .#ssc-server`, then
  `nix run .#ssc-client -- --connect host:port`.
- Saves: both local play and the dedicated server use `ssc-world.bin` by
  default; pass `--world PATH` to select another save file. Player state
  (inventory, position, vitals, Hunger-effect duration, exhaustion and selected
  slot) is saved by an anonymous
  client UUID stored at
  `$XDG_DATA_HOME/super-simple-cubes/anonymous-player-id` (or
  `~/.local/share/super-simple-cubes/anonymous-player-id`). Delete that file
  to use a new identity. Dropped items and mobs are saved alongside the world
  file.
  The UUID is not authentication; authenticated accounts are not implemented.
- Textured cube world (procedural placeholder atlas), server-authoritative
  20 TPS physics, dig/place with server-side raycasts, server-driven sound
  triggers (logged), frustum + face culling, first-cut Distant
  Horizons-style LOD.
- Survival crafting: open inventory with E, then press 1 for 4 planks from a
  log, 2 for 4 sticks from 2 planks, 3 for a wooden pickaxe (3 planks +
  2 sticks), or 4 for a stone pickaxe (3 cobblestone + 2 sticks). Drag the
  pickaxe into a hotbar slot to mine stone and ores for drops. This is an
  interim inventory-wide recipe UI, not the full Minecraft crafting grid.
  Pickaxes wear out and remaining uses appear in the inventory.
- Q drops one item from the selected survival hotbar stack into the world.

See `implemented-features.md` for the full parity tracker.
