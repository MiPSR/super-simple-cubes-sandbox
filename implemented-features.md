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
- `partial` — Optional atomic world saves (`--world PATH`): deterministic
  seed, day/night clock and block edits, autosaved once a minute and on
  shutdown. Version-1 saves migrate with the default morning time. Anonymous
  player state persists by client-generated UUID (inventory, position, vitals,
  Hunger-effect duration, exhaustion and selected hotbar slot); legacy player
  saves migrate with default effect state. Dropped stacks persist with physics state;
  mob kind, health, position and AI timers persist too. Mobs and dropped
  items pause while no players are online; distant dropped items also pause
  until a player approaches. Collision terrain is generated
  before the first tick at a restored player position. UUIDs are identifiers,
  not authentication.
- `dev` — Anonymous identity: a UUIDv4 is generated once per client data
  directory and sent in the protocol handshake. Dedicated servers persist
  per-UUID inventories and reject simultaneous connections claiming the same
  ID. This is not secure authentication; password-based accounts are deferred.
- `partial` — Multi-player: one shared authoritative world, one session
  per client (dedicated accepts clients on a background thread), 20 Hz
  entity-state broadcast (own id included), per-player streaming and LOD
  areas, placement rejected inside any player, EntityRemove on
  disconnect. Placeholder box avatars (legs/body/head) placed by per-draw
  model matrices; player skins, name tags and entity culling granularity
  pending.

## World & Terrain

- `partial` — World shape: 16×16 columns, 16³ sub-chunks, height −64..320
  (modern Java shape, Bedrock sub-chunk granularity); wire/streaming layout
  is our own.
- `partial` — Terrain generation: 4-octave simplex fbm heightmap, bedrock
  floor, grass/dirt/sand/sandstone/snow layers by height; caves carved by
  seeded 3D simplex (blobby tunnels, surface/bedrock crust preserved);
  deterministic oak trees (~1 per 16 columns, canopies cross column
  borders consistently via neighbor scan); water fills basins up to sea
  level. Below Y=0 the stone body becomes deepslate, with a mixed transition
  from Y=0 through Y=7. Seeded coal and iron ore clusters replace stone/deepslate in depth-limited
  bands (coal Y≥0, iron Y≤64) and drop coal/raw iron, respectively. Ore
  frequency and shapes are approximate; small gravel pockets also replace
  the underground stone/deepslate body. Deepslate iron ore exists, but
  other ores, biomes, structures and flowing water are pending.
- `partial` — Water: static lakes rendered translucent (blend pass,
  back-to-front, no depth writes), same-type face merging; swimming
  physics (slow sink, jump-to-rise, drag). No flow simulation, waves or
  underwater fog yet; cave pockets under lakes stay dry.
- `dev` — 20 placeholder blocks with procedurally generated textures in
  Minecraft's 16×16 tile atlas convention; real texture packs load via
  `--textures <256×256 atlas.png>` (PNG decode through the approved
  `png` crate) — we cannot ship Mojang assets (specs §1).
- `partial` — Coal and iron ore blocks (hardness 3) appear in generated
  stone and drop one coal/raw iron with the correct pickaxe tier.
  No fortune, silk touch, or iron smelting yet.
- `partial` — Deepslate (hardness 3) and deepslate iron ore (4.5)
  generate below Y=0; breaking deepslate drops cobbled deepslate and both
  iron ore variants drop raw iron with a stone pickaxe. Deepslate coal ore, deepslate variants
  of other ores and exact Bedrock distribution are pending.
- `partial` — Gravel deposits occur underground; breaking gravel drops flint
  at the wiki's unenchanted 10% chance, otherwise gravel. Gravel generation
  shape/distribution is approximate. Placed sand/gravel and stacks above
  broken supports settle immediately with persisted block updates (no
  animated falling entity, water displacement, suffocation or automatic
  updates of suspended worldgen deposits yet); no shovels or Fortune/Silk
  Touch yet.

## Player

- `1:1` (values) — Player physics constants: 0.6×1.8 AABB, eye 1.62,
  walk 4.317 / sprint 5.612 / sneak 1.31 blocks/s, gravity 32 b/s², jump apex
  ≈1.27 blocks, terminal velocity 78.4.
- `1:1` — Server tick rate 20 TPS.
- `partial` — Survival mode (`--mode survival|creative`, default survival):
  health 20 / hunger 20 / air 300 with wiki-accurate rules — fall damage
  (distance − 3), drowning (1 heart/s after 15 s), exhaustion-driven hunger
  (sprint 4× faster), regen 1 HP/4 s at hunger ≥ 18, starvation stops at
  half a heart (normal difficulty), timed digging (hardness × 1.5, × 5
  for tool-required blocks without a pickaxe), block drops into a 36-slot
  inventory (stone→cobble, grass→dirt, leaves→apple at 5%), placing
  consumes items, eating (apple +4 hunger), death → auto-respawn after 2 s
  at spawn (Minecraft uses a death screen + manual respawn); death drops
  inventory as world items. Below Y=−64 survival players take 4 damage
  every 10 ticks (Bedrock-style void); an invisible floor at Y=−105 stops
  further descent. Void-floor landings do not add fall damage; logging out
  there restores a safe spawn position. Anonymous UUID saves preserve
  inventory, position, vitals and selected slot. No armor, XP,
  general crafting or mode switching mid-session yet.
- `partial` — Game modes: creative (default hotbar blocks, instant dig,
  no consumption, E = block picker, double-jump flight with server
  authority) and survival (empty start, E =
  inventory screen, drag stacks between slots with server-authoritative
  swaps and same-item stack merging.
- `partial` — Movement: axis-separated swept-AABB collision with sub-stepping
  (no tunneling), jump, sneak/sprint speeds and basic swimming; no
  momentum/drag simulation, auto step-up or water-aware mob AI yet.
- `partial` — Block interaction: creative instant dig / survival timed dig
  (server-side progress with re-target on look change, progress broadcast
  for the HUD), reach 4.5, server-side DDA voxel raycast, entity attacks
  blocked by solid terrain, placement
  rejected inside any player; no survival dig particles/crack overlay
  or additional tool types yet.
- `partial` — Wooden and stone pickaxes mine tool-required blocks at
  speed 2/4; stone/deepslate/coal require any pickaxe for a drop, and iron
  ore requires stone. The server offers four inventory-wide recipes:
  log→4 planks, 2 planks→4 sticks, 3 planks+2 sticks→wooden pickaxe,
  3 cobblestone+2 sticks→stone pickaxe. Survival inventory keys 1–4
  request these recipes, validated atomically by the server. Unlike wiki
  crafting, this does not require shaped grid placement or a crafting table;
  pickaxe durability and higher tool tiers are not implemented.
- `dev` — Sound triggers (dig/place/step) are chosen and sent by the server
  and flow end-to-end; playback via owner-approved **kira** with distance
  attenuation + stereo panning; `--sounds DIR` loads real OGG/WAV packs
  (`dig|place|step.*`), procedural placeholder WAVs are synthesized in
  code otherwise (no Mojang assets, specs §1). Headless machines fall
  back to log-only. Per-block/per-surface sound variety, kira spatial
  listeners and underwater muffling pending.
- `partial` — Spawn search: spirals out from the origin for dry land
  (≥2 blocks above sea level); real MC spawn is biome-aware.
- `partial` — Day/night cycle: server clock at Minecraft pace (24000
  ticks ≈ 20 min per cycle), broadcast once per second; the client maps
  time to a sky color (dawn/day/dusk/night keys — no lighting changes,
  per specs §3). No sun/moon rendering, sleep or clock item yet.
- `partial` — Item system: block-items 1:1 with block ids, apple, raw
  porkchop and rotten flesh; food restores 4/3/4 hunger respectively,
  with rotten flesh applying Hunger I for 30 s at an 80% chance. The HUD
  shows a text countdown for the effect (no icon yet). Server-owned dropped
  stacks have basic gravity, same-item
  merging, 10-tick pickup delay, 5-minute despawn and partial inventory
  pickup (including passing a partial stack to the next nearby player), with
  merged stacks retaining the longer pickup delay, plus
  gentle attraction toward nearby players. Client renders gentle
  bobbing and rotation; merge radius/timing refinements pending.
  36-slot inventories (9 hotbar + 27 storage, stack 64) are server-owned
  and synced. Sticks, coal and raw iron are non-block items.
- `partial` — Gamepad (owner-approved gilrs, engine-side input abstraction):
  left stick moves, right stick looks (rate-normalized), A jumps, RT digs,
  LT places, Start = Escape; dead-zoned, works alongside keyboard/mouse.
  Button remapping/UI, haptics and analog movement (protocol is digital
  keys) pending.
- `partial` — Mobs: server-authoritative pigs (passive wandering) and zombies
  (night spawning, chase, melee damage, knockback, daylight burning when
  exposed to sky); solid walls prevent zombie melee hits. Entity-kind
  packets and placeholder client meshes.
  Spawn/despawn rules and AI are simplified; no pathfinding, fire visuals,
  breeding, or full Bedrock mob roster. Pigs drop 1–3 raw porkchops,
  zombies 0–2 rotten flesh; drops use the basic item-entity system.

## Client

- `1:1` (infra) — Face culling at mesh time: only faces adjacent to
  non-opaque blocks are emitted; cross-column neighbor lookups so chunk
  borders are correct (meshing gated on 4 neighbors being streamed).
- `1:1` (infra) — Frustum culling per tight sub-chunk AABB (p/n-vertex,
  Gribb–Hartmann extraction), conservative (never culls visible geometry).
- `1:1` (infra) — Occlusion ("cave") culling, Minecraft-style: per-sub-chunk
  border masks (16-bit slices), BFS visibility flood from the camera's
  sub-chunk through open border pairs, plus a one-ring dilation so solid
  geometry adjacent to lit air still draws. Unknown sub-chunks are never
  traversed (bounded flood, no false culling during streaming). Verified
  on the surface: ~31% of near meshes occluded with a pixel-identical
  frame.
- `1:1` (infra) — Near meshes drawn front-to-back; GL back-face culling.
- `1:1` (infra) — HUD text rendering (owner-approved glyph_brush + embedded
  Monocraft OFL font): pixel-coordinate API, crosshair, server-driven
  hotbar labels, F3 debug overlay; depth-off, cull-off, blend pass after
  the world. glyph_brush's tex_coords are already-normalized UVs (the
  double-normalization bug made glyphs sample sub-texels — fixed and
  verified by pixel-probing).
- `partial` — One draw call per 16³ sub-chunk mesh; batching/merging pending
  (Sodium-style tricks are ours to design better, not copy, specs §3).
- `partial` — LOD (Distant Horizons-style, integrated from day one):
  server-streamed per-column summaries (4×4 cells of height+tile), region
  heightfield meshes with side skirts, near-cell skip matching the actual
  meshed extent (rd−1, no sky gap ring); richer LOD levels and fade
  pending.
- `partial` — Client-side view interpolation of the 20 Hz authoritative state
  and local look smoothing (presentation only, no client logic).
- `partial` — Chunk streaming sends raw u16 blocks (8 KiB/sub-chunk);
  palette + compression pending. The wire layout itself is single-sourced
  in `SubChunk::to_wire/from_wire` (no duplicated index math).
- `1:1` (infra) — Sub-chunk wire format defined once in `ssc-world`, used
  by both server encoder and client decoder (fixes the class of bug where
  two hand-written index orders drift apart).
- `1:1` (infra) — `ssc-lod` is atlas-layout-agnostic: tile→UV mapping is
  injected by the presentation layer (`ssc-engine::atlas_tile_uv`), the
  only place atlas layout exists.
- `1:1` (infra) — Dig/place/miss interaction paths pinned by server unit
  tests over the real wire codec (update + sound packets asserted);
  entity-state broadcast to every client and placement-inside-any-player
  rejection are pinned too.
- `dev` — Block dig/place fire on the press *event* (not held-state
  polling) with hold-repeat at 4 Hz: quick clicks never reach a held-state
  snapshot in a single frame, so event-driven was required; Minecraft's
  exact dig-timing model (progress, tools) is not implemented yet.
- `partial` — 128-144 block gap ring between the near ring edge and LOD
  skip-margin — CLOSED: the LOD skip margin now matches the actual meshed
  extent (interior columns, cheb ≤ rd−1).
