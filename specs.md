# Super Simple Cubes — Project Specification

This document is the single source of truth for the project. It is a cleaned-up
restatement of the founding requirements. **It must not change.** Everything
else — `agents.md`, `implemented-features.md`, the code — must comply with it.

## 1. Product

- A from-scratch Minecraft clone written **only in Rust**.
- Feature parity target: **Minecraft Bedrock (latest) game logic**, because its
  logic is the cleaner one, but with a Java-Edition-like feel/UX where the two
  editions differ. Where logic conflicts, Bedrock semantics win.
- Reference for game behavior: <https://minecraft.wiki/> (or another wiki if
  that one lacks the needed detail).
- **No Microsoft bloat**: no external login, accounts, store, marketplace, or
  telemetry. Anonymous players receive a client-generated UUID used only as a
  persistent identity for server-side player data; it does not authenticate or
  prove ownership. Password-based authenticated players are a later phase.
- **Mods are prohibited.** Anything that would be a mod must ship as a
  first-party feature of the codebase with an integrated UI/menu.

## 2. Architecture

- Single repository: one Cargo workspace, shared crates, two executables.
  Build selection (client / server / graphics backend) happens through Cargo
  features and workspace binaries — never through separate repos.
- **`ssc-server`** — the authoritative game server.
  - Runs **dedicated** (headless, over the network) or **local**
    (embedded in the client process for single-player).
  - Contains **all** game logic: world generation, physics, block updates,
    mobs, crafting, inventory, and **sound triggers**.
- **`ssc-client`** — the game.
  - **Zero game logic.** It is 100% server-driven: blocks, entities, events and
    sound triggers all come from the server. If the official Minecraft client
    works differently, we do not care.
  - Renders and sends input. Nothing else.
  - The client must **never call graphics/windowing APIs directly**; it goes
    through our own engine crate exclusively.
- Shared crates (`ssc-world`, `ssc-protocol`, `ssc-math`, `ssc-lod`) contain no
  gameplay logic, only data types both sides need.

## 3. Engine

- Custom trait-based rendering engine: `ssc-engine`.
- Backends: **OpenGL** and **Vulkan**, selectable via Cargo features
  (default: OpenGL).
- Visual baseline: **textured cubes**. No lighting, no shader effects until
  the baseline is correct and fast. Plain Minecraft-compatible textures on
  cube objects.
- Non-negotiable priorities, in order:
  1. **Correct culling** — face culling at meshing, frustum culling,
     occlusion/visibility culling. Culling is the most important part of the
     client.
  2. **Proper rendering.**
  3. **High framerate.**
- A **Distant Horizons-style LOD system** (<https://gitlab.com/distant-horizons-team/distant-horizons>)
  is part of the core design **from day one**, not an add-on.
- Java-side performance mods (Sodium, Lithium, FerriteCore, etc.) are
  intentionally **not** copied: we design our own better path.

## 4. Dependencies

- **Minimal library policy**: few dependencies, each ultra-popular and
  well-maintained on GitHub. Popular alternatives must be compared.
- Audio / video / gamepad / other exotic dependencies require explicit owner
  approval first, with a comparison of the options.
- Prefer `std`-only implementations (threads, channels, hand-rolled binary
  codecs) wherever reasonable.

## 5. Sparkle-Morpher

- Support <https://github.com/sdf123098/Sparkle-Morpher> models inside our
  system, with an **integrated in-game menu** (not a mod).
- Phase: **later**, after core gameplay works. The mesh/model pipeline must
  stay generic enough to allow it.

## 6. Deliverable & tooling

- Target platform: **NixOS** (Linux, x86_64 first).
- Nix flake at the repo root providing:
  - `nix run .#ssc-client` and `nix run .#ssc-server`
  - `nix build .#ssc-client`, `nix build .#ssc-server`
  - dev shell: `nix develop`
- The host has **no Rust toolchain**: all building and testing goes through
  Nix.

## 7. Process

- Commit **and push** after every completed chunk of work.
- Do **not** compile after every small change. Compile at meaningful
  milestones (a big chunk of code is done) and fix errors in batch.
- When a genuine technical decision needs the owner, ask one focused question
  with options; otherwise proceed autonomously.
- For every wiki feature implemented, add a line to
  `implemented-features.md`. If a feature is **not** 1:1 with the wiki — or was
  deliberately made different per this spec — the line must say so and why.
