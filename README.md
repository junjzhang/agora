# agora

Agent-era workspace manager for the [niri](https://github.com/YaLTeR/niri) Wayland compositor.

agora maps projects to niri workspaces, tracks AI agent sessions (Claude Code, Codex) via hooks, and provides a keyboard-driven picker for navigating and launching tools.

## Architecture

```
┌──────────────────┐     ┌──────────────────┐     ┌──────────────────┐
│  agora (CLI)     │────▶│  agorad (daemon) │◀────│  agent hooks     │
│  agora picker    │     │  Unix socket IPC │     │  (Claude / Codex)│
│  DMS bar plugin  │     │  niri events     │     │                  │
└──────────────────┘     │  SSH tunnels     │     └──────────────────┘
                         └──────────────────┘
```

- **agorad** — daemon that manages projects, workspaces, agents, and remote tunnels
- **agora** — CLI client for all operations
- **agora-picker** — standalone Quickshell layer-shell UI (Mod+O / Mod+Shift+O)
- **agoraWorkspaces** — DMS bar plugin showing workspace pills with agent status

## Install

```sh
cargo build --release
cp target/release/agora target/release/agorad ~/.local/bin/

# systemd user service
cp dist/agorad.service ~/.config/systemd/user/
systemctl --user enable --now agorad

# picker (Quickshell)
cp -r ui/picker ~/.config/quickshell/agora-picker

# DMS bar plugin
cp -r ui/dms-plugins/agoraWorkspaces ~/.config/DankMaterialShell/plugins/

# install hooks into Claude Code / Codex
agora hook install --cli claude
agora hook install --cli codex
```

## Usage

```sh
# project management
agora add myproject /path/to/project --launcher terminal --launcher claude
agora open myproject              # focus or create workspace, spawn launchers
agora list                        # list projects
agora edit myproject              # edit project spec in $EDITOR
agora forget myproject            # remove project (keeps workspace)

# promote current workspace to a project
agora promote --name myproject .

# remote projects via SSH tunnel
agora remote add gpu.coder
agora remote install gpu.coder    # push binary + install hooks on remote
agora add myremote /path --host gpu.coder

# agent sessions
agora agents --json               # list tracked agent sessions
agora focus-agent <session-id>    # focus the agent's terminal window

# picker actions (used by picker UI)
agora actions project myproject   # list available actions
agora run-action project myproject launcher:claude:new
```

## Config

`~/.config/agora/config.toml` patches built-in defaults. Only specify what you want to change.

```toml
cleanup_all_workspaces = true

# add extra args to the claude launcher (applies to all claude actions)
[launchers.claude]
default_args = ["--dangerously-skip-permissions"]

# add a custom launcher
[launchers.mytool]
label = "Debug Tool"
group = "TOOLS"
local = ["kitty", "--directory", "{path}", "--", "zsh", "-ic", "mytool {args}; exec zsh"]

[launchers.mytool.actions.run]
label = "Run Debug Tool"
key = "⌥D"
args = ["--verbose"]
```

### Launcher variables

| Variable | Expands to |
|---|---|
| `{path}` | Project root path |
| `{host}` | SSH host alias (remote only) |
| `{args}` | `default_args` + action-specific `args`, shell-quoted |

### Action conditions (`when`)

| Condition | Shows when |
|---|---|
| `local` | Root has no host (local project) |
| `remote` | Root has a host |
| `has_agent` | Any agent session in the project |
| `has_agent:claude` | Claude session in the project |
| `has_agent:codex` | Codex session in the project |
| `never` | Never (disabled) |

## Layout

```
src/
  lib.rs              # crate root
  model.rs            # Project, AgentSession, AgentPhase, etc.
  ipc.rs              # Request/Response wire protocol
  store.rs            # JSON persistence (projects, remotes)
  bin/
    agora.rs           # CLI
    agorad/            # daemon
      main.rs          # socket server, dispatch
      config.rs        # TOML config, launcher registry, defaults
      actions.rs       # action list + execution for picker
      project.rs       # project CRUD + niri workspace ops
      hooks.rs         # agent state machine, PID→window mapping
      tunnel.rs        # SSH reverse-forward tunnel lifecycle
      launcher.rs      # template expansion, process spawn
      niri.rs          # niri event loop, workspace tracking
ui/
  picker/              # Quickshell picker (Mod+O / Mod+Shift+O)
  dms-plugins/
    agoraWorkspaces/   # DMS bar widget with agent status
dist/
  agorad.service       # systemd user unit
```

## Agent tracking

agora observes agents via hooks — it never sends commands to them. When Claude Code or Codex fires a hook event (SessionStart, UserPromptSubmit, PreToolUse, Stop, etc.), agora updates the session's phase, model, effort level, current tool, and turn count.

The picker and bar widget display this state:
- Phase dot: green (running), red (needs input), orange (needs permission), gray (idle)
- Model badge: opus (purple), sonnet (blue), haiku (green), gpt-4 (teal), gpt-5 (orange)
- Effort badge: the thinking effort level from CLI settings
- Current tool: what the agent is executing right now

## License

MIT OR Apache-2.0
