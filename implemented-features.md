# Implemented Features

Every implemented wiki feature gets a line here. Status legend:

- `1:1` — matches <https://minecraft.wiki/> behavior.
- `partial` — implemented but simplified or incomplete; reason given.
- `dev` — intentionally different per `specs.md`; reason given.

## Infrastructure & Engine

- `dev` — Custom Rust engine (`ssc-engine`) instead of Minecraft's renderer:
  by design, we own the whole pipeline (specs §3).
- `dev` — No login / store / marketplace / telemetry: Microsoft bloat removed
  by spec (specs §1).
- `dev` — Client is 100% server-driven, including sound triggers: differs
  from the official client on purpose (specs §2).
- `1:1` (infra) — Cargo workspace with shared crates, client and server
  selected via Cargo features/binaries.
- `1:1` (infra) — Nix flake with `ssc-client` and `ssc-server` packages,
  runnable via `nix run`, dev shell via `nix develop` (specs §6).
- `1:1` (infra) — winit + glutin + glow OpenGL backend (owner-approved stack);
  ash Vulkan backend planned.
- `1:1` (infra) — Local play (in-process server, memory wire) and dedicated
  server (TCP wire) from the same server library.

## World & Terrain

- `partial` — World shape: 16×16 columns, 16³ sub-chunks, height −64..320
  (modern Java shape, Bedrock sub-chunk granularity); wire/streaming layout
  is our own.
- `partial` — Terrain generation: 4-octave simplex fbm heightmap, bedrock
  floor, grass/dirt/sand/sandstone/snow layers by height. No caves, trees,
  biomes, structures or water yet.
- `dev` — 14 placeholder blocks with procedurally generated textures in
  Minecraft's 16×16 tile atlas convention; real texture-pack (PNG) loading
  planned — we cannot ship Mojang assets (specs §1).

## Player

- `1:1` (values) — Player physics constants: 0.6×1.8 AABB, eye 1.62,
  walk 4.317 / sprint 5.612 / sneak 1.31 blocks/s, gravity 32 b/s², jump apex
  ≈1.27 blocks, terminal velocity 78.4.
- `1:1` — Server tick rate 20 TPS.
- `partial` — Movement: axis-separated swept-AABB collision with sub-stepping
  (no tunneling), jump, sneak/sprint speeds; no momentum/drag simulation,
  auto step-up, swimming or fall damage yet.
- `partial` — Block interaction: creative-style instant dig/place, reach 4.5,
  server-side DDA voxel raycast, placement rejected inside the player AABB;
  no survival dig timing, tools, drops or block-specific behaviors yet.
- `dev` — Sound triggers (dig/place) are chosen and sent by the server and
  flow end-to-end; the client logs them until an audio backend is approved
  (specs §4).
- `partial` — Fixed 9-slot hotbar with server-defined blocks; full inventory,
  survival mode, hunger and health pending.

## Client

- `1:1` (infra) — Face culling at mesh time: only faces adjacent to
  non-opaque blocks are emitted; cross-column neighbor lookups so chunk
  borders are correct (meshing gated on 4 neighbors being streamed).
- `1:1` (infra) — Frustum culling per tight sub-chunk AABB (p/n-vertex,
  Gribb–Hartmann extraction), conservative (never culls visible geometry).
- `1:1` (infra) — Near meshes drawn front-to-back; GL back-face culling.
- `partial` — One draw call per 16³ sub-chunk mesh; batching/merging pending
  (Sodium-style tricks are ours to design better, not copy, specs §3).
- `partial` — LOD (Distant Horizons-style, integrated from day one):
  server-streamed per-column summaries (4×4 cells of height+tile), region
  heightfield meshes with side skirts, near-cell skip to avoid z-fighting;
  richer LOD levels and fade pending.
- `partial` — Client-side view interpolation of the 20 Hz authoritative state
  and local look smoothing (presentation only, no client logic).
- `partial` — Chunk streaming sends raw u16 blocks (8 KiB/sub-chunk);
  palette + compression pending.
- `partial` — 128-144 block gap ring between the near ring edge and LOD
  skip-margin; to be closed with an overlapping margin.
