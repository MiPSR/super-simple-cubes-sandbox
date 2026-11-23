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
  (inventory, position, vitals, Hunger-effect duration, exhaustion, saturation and selected
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
  2 sticks), or 4 for a stone pickaxe (3 cobblestone + 2 sticks).
  Keys 5/6 craft wooden/stone axes (3 planks/cobblestone + 2 sticks);
  7/8 craft wooden/stone shovels (1 plank/cobblestone + 2 sticks). Drag a
  tool into the hotbar: pickaxes mine stone/ores, axes chop wood faster,
  and shovels dig soil faster and collect snowballs from snow blocks. This is an
  interim inventory-wide recipe UI, not the full Minecraft crafting grid.
  Tools wear out and remaining uses appear in the inventory.
- To repair a tool, select its hotbar slot, open E and press 9. Another tool
  of the exact same type is consumed, combining remaining uses plus a 5% bonus.
- Right-click oak logs with an axe to strip them; stripped logs still craft
  into planks.
- Hold right-click for 1.6 seconds to eat; releasing use or switching items
  cancels eating. Eating progress is shown under the crosshair.
- Breaking or decaying oak leaves can drop saplings. Plant them on dirt or
  grass in daylight with room above; they grow into new oaks over time.
- In the inventory, press `0` to craft a furnace from eight cobblestone.
  Place it and right-click to open; drag raw iron/copper, pork or logs into
  input and coal, charcoal, wood or sticks into fuel. Drag finished output
  back to your inventory. `E` or Escape closes the furnace.
- In the inventory, `Shift+3`, `Shift+5`, and `Shift+7` craft an iron
  pickaxe, axe, and shovel from iron ingots and sticks.
- `Shift+1`, `Shift+2`, and `Shift+4` craft wooden, stone, and iron swords
  from two planks/cobblestone/ingots and one stick.
- Furnaces also turn sand into glass, cobblestone into stone, stone into
  smooth stone, and cobbled deepslate into deepslate. Glass breaks without
  dropping an item.
- Q drops one item from the selected survival hotbar stack into the world;
  Ctrl+Q drops the entire stack.

See `implemented-features.md` for the full parity tracker.
