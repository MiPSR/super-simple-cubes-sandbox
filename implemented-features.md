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
