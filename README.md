# agora — niri workspace manager

Agent-era workspace manager for the [niri](https://github.com/YaLTeR/niri) compositor.

> 🚧 v0.1 / MVP scaffolding — design lives in [`VISION.html`](VISION.html).

## Quick start

```sh
pixi run build             # cargo build
pixi run agora -- --help   # CLI
pixi run agorad            # daemon (stub)
pixi run check             # fmt + clippy + test
```

## Layout

- `src/lib.rs` + `src/model.rs` — shared model (Project / Root / Launcher)
- `src/bin/agorad.rs` — daemon
- `src/bin/agora.rs` — CLI

## Action/Launcher Config

`agorad` owns picker actions. The picker asks the daemon for actions and then
calls back with the selected action id; it does not build launcher commands.

User config lives in `~/.config/agora/config.toml` and patches the built-in
defaults:

```toml
cleanup_all_workspaces = true

[launchers.claude]
default_args = ["--dangerously-skip-permissions"]

[launchers.claude.actions.continue]
key = "⌥C"
when = "has_agent:claude"

[launchers.codex]
default_args = ["--dangerously-bypass-approvals-and-sandbox"]

[launchers.codex.actions.continue]
key = "⌥⇧X"
when = "has_agent:codex"

[launchers.mytool]
label = "Debug Tool"
group = "TOOLS"
local = ["kitty", "--directory", "{path}", "--", "zsh", "-ic", "mytool {args}; exec zsh"]

[launchers.mytool.actions.open]
label = "Run Debug Tool"
key = "⌥D"
args = ["--fast"]
```

Supported action conditions are `local`, `remote`, `has_agent`,
`has_agent:claude`, `has_agent:codex`, and `never`.
