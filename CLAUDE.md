# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

**agora** — agent-era workspace manager for the niri Wayland compositor. Maps projects to niri workspaces, tracks AI agent sessions (Claude Code, Codex) via hooks, and provides a keyboard-driven picker.

## Build & Test

Managed by pixi (conda-forge rust toolchain). All commands via `pixi run`:

```sh
pixi run build           # cargo build
pixi run build-release   # cargo build --release
pixi run test            # cargo test
pixi run fmt             # cargo fmt
pixi run fmt-check       # cargo fmt --check
pixi run lint            # cargo clippy --all-targets -- -D warnings
pixi run check           # fmt-check + lint + test (CI gate)
pixi run install         # cargo install --path . --bins --locked
```

Run a single test: `pixi run -- cargo test <test_name>`

Run the daemon locally: `pixi run agorad` (listens on `$XDG_RUNTIME_DIR/agora.sock`)

## Architecture

Two binaries sharing a library crate:

```
src/lib.rs        → pub mod {ipc, model, store}
src/bin/agora.rs  → CLI client (thin: each subcommand = one IPC round-trip)
src/bin/agorad/   → daemon (socket server + niri event loop thread)
```

**Daemon threads:**
- Main thread: accepts unix socket connections, spawns a handler thread per client
- `niri-events` thread: subscribes to niri IPC event stream, maintains window claims and workspace state

**Shared state** (`Arc<Mutex<Inner>>`):
- `projects` — persisted to `~/.local/share/agora/projects.json` (versioned envelope, atomic write via tmp+rename)
- `claims` — window→metadata map rebuilt from niri events
- `workspaces` — niri workspace state
- `agents` — in-memory map of agent sessions keyed by session_id (also cached to `~/.cache/agora/agents.json`)
- `remotes` — SSH tunnel configs persisted to `~/.local/share/agora/remotes.json`
- `tunnels` — live tunnel state
- `launcher_registry` — built-in launcher templates + user patches from config.toml

**IPC protocol:** JSON-line over unix socket. Client writes one `Request`, daemon writes one `Response`. Connection closes after each round-trip.

**Agent tracking flow:** CLI hooks → `agora hook event <EVENT> --cli claude` → reads stdin JSON → enriches with host/pid/slug/model/effort → sends `Request::Hook` to daemon → `hooks::apply_hook` updates `AgentSession` state machine → writes cache.

**Config layering:** `~/.config/agora/config.toml` (user config) + `~/.config/agora/launchers.json` (legacy overrides). Launcher patches merge on top of built-in defaults in `config::default_launcher_registry()`.

## Key Design Decisions

- Store version is explicitly checked. Bump `STORE_VERSION` in `store.rs` for incompatible `Project` shape changes and add a migration path in `load()`.
- `niri-ipc` pinned to exact git rev matching the locally installed niri build. Must bump rev when niri-git updates.
- Agent sessions are observe-only: daemon never sends commands to agents, only receives hook events.
- PID→window mapping walks `/proc/{pid}/status` PPid chain upward (up to 30 hops) to find the enclosing terminal window in niri's claim map.
- Remote agent focus uses SSH process tree scanning to find terminal windows connected to a specific host.

## Config Paths (runtime)

| What | Path |
|------|------|
| Projects store | `~/.local/share/agora/projects.json` |
| Remotes store | `~/.local/share/agora/remotes.json` |
| User config | `~/.config/agora/config.toml` |
| Socket | `$XDG_RUNTIME_DIR/agora.sock` |
| Agents cache | `~/.cache/agora/agents.json` |
| systemd unit | `dist/agorad.service` |
