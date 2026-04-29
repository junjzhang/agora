//! agora — niri workspace manager CLI
//!
//! Thin client to agorad. Each subcommand maps to one IPC round-trip.
//! See VISION.html §8 for the full command surface.

use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use agora::ipc::{self, Payload, Request, Response};
use agora::model::{Launcher, ProjectSpec};

#[derive(Parser)]
#[command(
    name = "agora",
    version,
    about = "agent-era niri workspace manager",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum RemoteCmd {
    /// Register a remote host. Daemon brings up an SSH reverse-forward tunnel.
    Add {
        /// SSH alias / hostname (must resolve via your ~/.ssh/config)
        host: String,
    },
    /// Remove a remote host (also tears down its tunnel).
    Remove {
        host: String,
    },
    /// List configured remotes with tunnel status.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Push the agora binary to the remote and install hooks there.
    Install {
        host: String,
    },
}

#[derive(Subcommand)]
enum HookCmd {
    /// Install hook entries into the agent CLI's settings file
    Install {
        /// Which CLI to wire up
        #[arg(long, default_value = "claude")]
        cli: String,
        /// Embed AGORA_HOST=<alias> in hook commands so agents report the
        /// SSH alias rather than the machine's /etc/hostname. Used by
        /// `agora remote install` automatically.
        #[arg(long)]
        host_alias: Option<String>,
    },
    /// Remove agora hook entries from the agent CLI's settings file
    Uninstall {
        #[arg(long, default_value = "claude")]
        cli: String,
    },
    /// Adapter target for hook commands. Reads JSON from stdin,
    /// forwards to daemon. Invoked by the CLI agent itself.
    Event {
        /// Hook event name (PreToolUse / Stop / Notification / ...)
        event: String,
        /// CLI source (claude / codex)
        #[arg(long, default_value = "claude")]
        cli: String,
    },
}

#[derive(Subcommand)]
enum Cmd {
    /// Add a project rooted at PATH (default: current directory)
    Add {
        /// Project name (becomes its id and default workspace name)
        name: String,
        /// Root directory (remote path if --host is given)
        #[arg(default_value = ".")]
        path: String,
        /// Mark the root as remote (e.g. `gpu.coder`). PATH is not validated locally.
        #[arg(long, value_name = "HOST")]
        host: Option<String>,
        /// Add a launcher to spawn on `agora open`. May be repeated.
        ///
        /// Format: `vscode` | `zed` | `kitty` | `kitty:CMD` | `claude` | `codex`
        #[arg(long = "launcher", value_name = "KIND", value_parser = parse_launcher)]
        launchers: Vec<Launcher>,
    },
    /// List all projects
    List {
        /// Emit JSON (one Project per array element) instead of the human table
        #[arg(long)]
        json: bool,
    },
    /// Focus the project's niri workspace; claim the current workspace if not yet named
    Open {
        /// Project name
        name: String,
    },
    /// Show daemon state — projects + currently-claimed windows
    Status,
    /// Remove a project from the manager (does not touch its niri workspace)
    Forget {
        /// Project name
        name: String,
    },
    /// Rename a project; also renames the matching niri workspace if present
    Rename {
        /// Current project name
        from: String,
        /// New project name
        to: String,
    },
    /// Open the project's spec in $EDITOR; daemon validates and applies on save
    Edit {
        /// Project name
        name: String,
    },
    /// Hook adapter / installer — call from CLI agent hooks
    #[command(subcommand)]
    Hook(HookCmd),
    /// List active agent sessions known to the daemon
    Agents {
        /// Emit JSON instead of human table
        #[arg(long)]
        json: bool,
    },
    /// Focus the terminal window containing an agent session
    FocusAgent {
        /// Agent session id (from `agora agents`)
        session_id: String,
    },
    /// Manage remote hosts (SSH reverse-forward tunnels)
    #[command(subcommand)]
    Remote(RemoteCmd),
    /// Bind the focused niri workspace to an existing project
    Attach {
        /// Project name
        name: String,
        /// Rename/claim the focused ws to project's workspace_name
        /// (default: update project's workspace_name to match the ws's name)
        #[arg(long)]
        rename_ws: bool,
    },
    /// Promote the currently focused niri workspace into a project
    Promote {
        /// Project id override. Default: focused ws name → basename(PATH)
        #[arg(long)]
        name: Option<String>,
        /// Root directory (default: current dir)
        #[arg(default_value = ".")]
        path: String,
        /// Mark the root as remote (e.g. `gpu.coder`).
        #[arg(long, value_name = "HOST")]
        host: Option<String>,
        /// Add a launcher. May be repeated.
        ///
        /// Format: `vscode` | `zed` | `kitty` | `kitty:CMD` | `claude` | `codex`
        #[arg(long = "launcher", value_name = "KIND", value_parser = parse_launcher)]
        launchers: Vec<Launcher>,
        /// If --name conflicts with the focused ws's existing name, rename the ws.
        #[arg(long)]
        rename_ws: bool,
    },
}

fn main() -> Result<()> {
    // Restore default SIGPIPE handling so `agora ... | head` doesn't panic
    // when the downstream pipe closes — standard Unix CLI behavior.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let cli = Cli::parse();
    match cli.command {
        Cmd::Add {
            name,
            path,
            host,
            launchers,
        } => {
            // Daemon canonicalize uses its own cwd (likely `/`), so resolve
            // relative paths against the user's cwd here. Skip for remote roots.
            let root_path = if host.is_some() {
                path
            } else {
                resolve_local_path(&path)?
            };
            let payload = call(Request::Add {
                name,
                root_path,
                host,
                launchers,
            })?;
            let Payload::Project(p) = payload else {
                anyhow::bail!("unexpected payload from daemon: {payload:?}");
            };
            let kinds: Vec<&'static str> = p.roots[0].launchers.iter().map(|l| l.kind()).collect();
            let host_part = match p.roots[0].host.as_deref() {
                Some(h) => format!(" @{h}"),
                None => String::new(),
            };
            if kinds.is_empty() {
                println!("added: {} -> {}{}", p.id, p.roots[0].path, host_part);
            } else {
                println!(
                    "added: {} -> {}{} (launchers: {})",
                    p.id,
                    p.roots[0].path,
                    host_part,
                    kinds.join(", "),
                );
            }
        }
        Cmd::List { json } => {
            let payload = call(Request::List)?;
            let Payload::Projects(mut projects) = payload else {
                anyhow::bail!("unexpected payload from daemon: {payload:?}");
            };
            // MRU order: most recently active first.
            projects.sort_by(|a, b| b.ts_last_active.cmp(&a.ts_last_active));
            if json {
                serde_json::to_writer(std::io::stdout(), &projects)
                    .context("serialize projects")?;
                println!();
            } else if projects.is_empty() {
                println!("(no projects)");
            } else {
                for p in projects {
                    let path = p.roots.first().map(|r| r.path.as_str()).unwrap_or("?");
                    println!("{}\t{}", p.id, path);
                }
            }
        }
        Cmd::Open { name } => {
            let payload = call(Request::Open { name })?;
            let Payload::Opened {
                project,
                claimed_current,
            } = payload
            else {
                anyhow::bail!("unexpected payload from daemon: {payload:?}");
            };
            if claimed_current {
                println!(
                    "claimed current workspace as '{}' ({})",
                    project.workspace_name, project.id
                );
            } else {
                println!("focused workspace '{}'", project.workspace_name);
            }
        }
        Cmd::Forget { name } => {
            let payload = call(Request::Forget { name })?;
            let Payload::Project(p) = payload else {
                anyhow::bail!("unexpected payload from daemon: {payload:?}");
            };
            println!("forgot: {} (was rooted at {})", p.id, p.roots[0].path);
        }
        Cmd::Rename { from, to } => {
            let payload = call(Request::Rename {
                from: from.clone(),
                to,
            })?;
            let Payload::Renamed {
                project,
                niri_ws_renamed,
            } = payload
            else {
                anyhow::bail!("unexpected payload from daemon: {payload:?}");
            };
            if niri_ws_renamed {
                println!(
                    "renamed: {} -> {} (niri workspace also renamed)",
                    from, project.id
                );
            } else {
                println!("renamed: {} -> {}", from, project.id);
            }
        }
        Cmd::Edit { name } => edit(name)?,
        Cmd::Hook(HookCmd::Install { cli, host_alias }) => {
            hook_install(&cli, host_alias.as_deref())?
        }
        Cmd::Hook(HookCmd::Uninstall { cli }) => hook_uninstall(&cli)?,
        Cmd::Hook(HookCmd::Event { event, cli }) => hook_event(&event, &cli)?,
        Cmd::FocusAgent { session_id } => {
            let payload = call(Request::FocusAgent { session_id })?;
            if !matches!(payload, Payload::Ack) {
                anyhow::bail!("unexpected payload from daemon: {payload:?}");
            }
        }
        Cmd::Remote(RemoteCmd::Add { host }) => remote_add(&host)?,
        Cmd::Remote(RemoteCmd::Remove { host }) => remote_remove(&host)?,
        Cmd::Remote(RemoteCmd::List { json }) => remote_list(json)?,
        Cmd::Remote(RemoteCmd::Install { host }) => remote_install(&host)?,
        Cmd::Agents { json } => {
            let payload = call(Request::Agents)?;
            let Payload::Agents(agents) = payload else {
                anyhow::bail!("unexpected payload from daemon: {payload:?}");
            };
            if json {
                serde_json::to_writer(std::io::stdout(), &agents).context("serialize agents")?;
                println!();
            } else if agents.is_empty() {
                println!("(no agents)");
            } else {
                for a in agents {
                    let phase = match a.phase {
                        agora::model::AgentPhase::Idle => "idle",
                        agora::model::AgentPhase::Running => "running",
                        agora::model::AgentPhase::WaitingInput => "waiting-input",
                        agora::model::AgentPhase::WaitingPermission => "waiting-permission",
                    };
                    let proj = a.project.as_deref().unwrap_or("-");
                    let msg = a
                        .last_message
                        .as_deref()
                        .map(|s| format!(" \"{s}\""))
                        .unwrap_or_default();
                    println!("[{}] {phase} project={proj}{msg}", a.session_id);
                }
            }
        }
        Cmd::Attach { name, rename_ws } => {
            let payload = call(Request::Attach {
                name: name.clone(),
                rename_ws,
            })?;
            let Payload::Project(p) = payload else {
                anyhow::bail!("unexpected payload from daemon: {payload:?}");
            };
            println!(
                "attached: project {} <-> workspace '{}'",
                p.id, p.workspace_name
            );
        }
        Cmd::Promote {
            name,
            path,
            host,
            launchers,
            rename_ws,
        } => {
            let root_path = if host.is_some() {
                path
            } else {
                resolve_local_path(&path)?
            };
            let payload = call(Request::Promote {
                name,
                root_path,
                host,
                launchers,
                rename_ws,
            })?;
            let Payload::Project(p) = payload else {
                anyhow::bail!("unexpected payload from daemon: {payload:?}");
            };
            let kinds: Vec<&'static str> = p.roots[0].launchers.iter().map(|l| l.kind()).collect();
            let host_part = match p.roots[0].host.as_deref() {
                Some(h) => format!(" @{h}"),
                None => String::new(),
            };
            if kinds.is_empty() {
                println!(
                    "promoted: {} -> {}{} (workspace: '{}')",
                    p.id, p.roots[0].path, host_part, p.workspace_name
                );
            } else {
                println!(
                    "promoted: {} -> {}{} (workspace: '{}', launchers: {})",
                    p.id,
                    p.roots[0].path,
                    host_part,
                    p.workspace_name,
                    kinds.join(", "),
                );
            }
        }
        Cmd::Status => {
            let payload = call(Request::Status)?;
            let Payload::Status {
                project_count,
                windows,
                agents,
            } = payload
            else {
                anyhow::bail!("unexpected payload from daemon: {payload:?}");
            };
            println!("projects: {project_count}");
            if windows.is_empty() {
                println!("windows: (none)");
            } else {
                println!("windows: {}", windows.len());
                for w in windows {
                    let project = w.project.as_deref().unwrap_or("-");
                    let app = w.app_id.as_deref().unwrap_or("?");
                    let ws = match (w.workspace_id, w.workspace_idx, w.workspace_name.as_deref()) {
                        (Some(id), Some(idx), Some(name)) => {
                            format!("{id}(idx={idx}, name=\"{name}\")")
                        }
                        (Some(id), Some(idx), None) => format!("{id}(idx={idx})"),
                        (Some(id), None, Some(name)) => format!("{id}(name=\"{name}\")"),
                        (Some(id), None, None) => format!("{id}"),
                        _ => "-".to_string(),
                    };
                    let col = w
                        .column
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "-".into());
                    let title = w.title.as_deref().unwrap_or("");
                    println!(
                        "  [{}] ws={} col={} app={} project={} \"{}\"",
                        w.window_id, ws, col, app, project, title
                    );
                }
            }
            if !agents.is_empty() {
                println!("agents: {}", agents.len());
                for a in agents {
                    let cwd = a.cwd.as_deref().unwrap_or("?");
                    let last = a.last_event.as_deref().unwrap_or("-");
                    let cli = match a.cli {
                        agora::model::AgentCli::Claude => "claude",
                        agora::model::AgentCli::Codex => "codex",
                    };
                    let phase = match a.phase {
                        agora::model::AgentPhase::Idle => "idle",
                        agora::model::AgentPhase::Running => "running",
                        agora::model::AgentPhase::WaitingInput => "waiting-input",
                        agora::model::AgentPhase::WaitingPermission => "waiting-permission",
                    };
                    let msg = a
                        .last_message
                        .as_deref()
                        .map(|s| format!(" \"{s}\""))
                        .unwrap_or_default();
                    println!(
                        "  [{}] {cli} phase={phase} last={last} cwd={cwd}{msg}",
                        a.session_id
                    );
                }
            }
        }
    }
    Ok(())
}

fn edit(name: String) -> Result<()> {
    // 1. Fetch current project from daemon.
    let payload = call(Request::Get { name: name.clone() })?;
    let Payload::Project(project) = payload else {
        bail!("unexpected payload from daemon: {payload:?}");
    };
    let original = serde_json::to_string_pretty(&project.spec()).context("serialize spec")?;

    // 2. Write to a tmpfile.
    let tmp_path = std::env::temp_dir().join(format!(
        "agora-edit-{}-{}.json",
        sanitize(&name),
        std::process::id(),
    ));
    {
        let mut f = std::fs::File::create(&tmp_path)
            .with_context(|| format!("create tmpfile {}", tmp_path.display()))?;
        f.write_all(original.as_bytes())
            .with_context(|| format!("write {}", tmp_path.display()))?;
    }

    // 3. Spawn editor via shell so $EDITOR can be a multi-word command.
    let editor = std::env::var("VISUAL")
        .ok()
        .or_else(|| std::env::var("EDITOR").ok())
        .unwrap_or_else(|| "vi".to_string());
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!(r#"{editor} "$@""#))
        .arg("--")
        .arg(&tmp_path)
        .status()
        .with_context(|| format!("invoke editor {editor}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&tmp_path);
        bail!("editor exited non-zero ({status}); aborted");
    }

    // 4. Read back and diff.
    let mut edited = String::new();
    std::fs::File::open(&tmp_path)
        .with_context(|| format!("reopen {}", tmp_path.display()))?
        .read_to_string(&mut edited)
        .with_context(|| format!("read {}", tmp_path.display()))?;
    if edited.trim() == original.trim() {
        let _ = std::fs::remove_file(&tmp_path);
        println!("no changes");
        return Ok(());
    }

    // 5. Parse + send Update. On failure, keep tmpfile so user can retry.
    let spec: ProjectSpec = match serde_json::from_str(&edited) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("parse failed: {e}");
            eprintln!("your edits are preserved at: {}", tmp_path.display());
            bail!("invalid JSON; not applied");
        }
    };
    let payload = match call(Request::Update {
        name: name.clone(),
        spec,
    }) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("daemon rejected update: {e}");
            eprintln!("your edits are preserved at: {}", tmp_path.display());
            return Err(e);
        }
    };
    let Payload::Project(p) = payload else {
        bail!("unexpected payload from daemon: {payload:?}");
    };
    let _ = std::fs::remove_file(&tmp_path);
    println!("updated: {}", p.id);
    Ok(())
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect()
}

fn hook_event(event: &str, cli: &str) -> Result<()> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("read stdin")?;
    // Hooks may invoke us with empty stdin (e.g. quick test). Treat as empty obj
    // so we can still inject agora_host below.
    let mut payload: serde_json::Value = if buf.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(&buf).context("parse hook stdin as JSON")?
    };
    if let Some(obj) = payload.as_object_mut() {
        // Host this hook is running on.
        if !obj.contains_key("agora_host") {
            if let Some(name) = read_hostname() {
                obj.insert("agora_host".to_string(), serde_json::Value::String(name));
            }
        }
        // PID of the process that invoked the hook (claude's fork). Daemon
        // walks up the process tree from here to find the terminal window.
        if !obj.contains_key("agora_pid") {
            let ppid = unsafe { libc::getppid() };
            if ppid > 1 {
                obj.insert(
                    "agora_pid".to_string(),
                    serde_json::Value::Number(ppid.into()),
                );
            }
        }
    }
    // Fire-and-forget: send and don't fail the hook on daemon error.
    // Hook scripts must exit cleanly so the agent CLI keeps moving.
    if let Err(e) = call(Request::Hook {
        cli: cli.to_string(),
        event: event.to_string(),
        payload,
    }) {
        eprintln!("agora hook: {e:#}");
    }
    Ok(())
}

fn read_hostname() -> Option<String> {
    // Prefer AGORA_HOST env (set by hook commands installed via
    // `agora hook install --host-alias X`). Falls back to /etc/hostname.
    if let Ok(v) = std::env::var("AGORA_HOST") {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return Some(v);
        }
    }
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `~/.claude/settings.json` for claude. Codex path differs but the install
/// surface is the same (settings.json + hooks).
fn hook_settings_path(cli: &str) -> Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME").context("HOME not set")?;
    let dir = match cli {
        "claude" => ".claude",
        "codex" => ".codex",
        other => anyhow::bail!("unknown cli '{other}' (expected claude|codex)"),
    };
    Ok(std::path::PathBuf::from(home).join(dir).join("settings.json"))
}

/// Marker substring: any hook command containing this is ours.
const HOOK_MARK: &str = "agora hook event";

/// Hook events we install for. Mirrors VibeHub's set, minus PermissionRequest
/// (we don't mediate decisions; user responds in their terminal).
fn hook_events_for(cli: &str) -> &'static [(&'static str, bool)] {
    // (event_name, needs_matcher_field)
    match cli {
        "claude" => &[
            ("SessionStart", false),
            ("SessionEnd", false),
            ("UserPromptSubmit", false),
            ("PreToolUse", true),
            ("PostToolUse", true),
            ("PermissionRequest", true),
            ("Notification", true),
            ("Stop", false),
            ("SubagentStop", false),
        ],
        _ => &[],
    }
}

fn hook_install(cli: &str, host_alias: Option<&str>) -> Result<()> {
    let path = hook_settings_path(cli)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("mkdir -p {}", parent.display()))?;
    }
    let mut data: serde_json::Value = if path.exists() {
        let buf = std::fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;
        if buf.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&buf)
                .with_context(|| format!("parse {} as JSON", path.display()))?
        }
    } else {
        serde_json::json!({})
    };
    if !data.is_object() {
        anyhow::bail!("{} root is not a JSON object", path.display());
    }
    let hooks = data
        .as_object_mut()
        .unwrap()
        .entry("hooks".to_string())
        .or_insert_with(|| serde_json::json!({}));
    if !hooks.is_object() {
        anyhow::bail!("hooks field in {} is not an object", path.display());
    }
    let hooks = hooks.as_object_mut().unwrap();

    let prefix = match host_alias {
        Some(alias) => format!("AGORA_HOST={alias} "),
        None => String::new(),
    };
    let cmd = format!("{prefix}agora hook event --cli {cli} {{event}}"); // placeholder; replaced per-event
    let events = hook_events_for(cli);
    if events.is_empty() {
        anyhow::bail!("no hooks defined for cli '{cli}' yet");
    }
    let mut added = 0;
    let mut already = 0;
    for (event_name, needs_matcher) in events {
        let event_cmd = cmd.replace("{event}", event_name);
        let entry = if *needs_matcher {
            serde_json::json!({
                "matcher": "*",
                "hooks": [{ "type": "command", "command": event_cmd }],
            })
        } else {
            serde_json::json!({
                "hooks": [{ "type": "command", "command": event_cmd }],
            })
        };
        let arr = hooks
            .entry(event_name.to_string())
            .or_insert_with(|| serde_json::json!([]));
        if !arr.is_array() {
            anyhow::bail!(
                "hooks.{event_name} in {} is not an array",
                path.display()
            );
        }
        let arr = arr.as_array_mut().unwrap();
        let already_present = arr.iter().any(|item| {
            item.get("hooks")
                .and_then(|h| h.as_array())
                .map(|hs| {
                    hs.iter().any(|h| {
                        h.get("command")
                            .and_then(|c| c.as_str())
                            .map(|s| s.contains(HOOK_MARK))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        });
        if already_present {
            already += 1;
        } else {
            arr.push(entry);
            added += 1;
        }
    }
    let buf = serde_json::to_vec_pretty(&data).context("serialize settings")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &buf).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    println!(
        "{}: {added} event(s) added, {already} already present",
        path.display()
    );
    Ok(())
}

fn hook_uninstall(cli: &str) -> Result<()> {
    let path = hook_settings_path(cli)?;
    if !path.exists() {
        println!("{}: not present, nothing to uninstall", path.display());
        return Ok(());
    }
    let buf = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    if buf.trim().is_empty() {
        return Ok(());
    }
    let mut data: serde_json::Value = serde_json::from_str(&buf)
        .with_context(|| format!("parse {} as JSON", path.display()))?;
    let Some(hooks) = data
        .as_object_mut()
        .and_then(|o| o.get_mut("hooks"))
        .and_then(|h| h.as_object_mut())
    else {
        println!("{}: no hooks section, nothing to do", path.display());
        return Ok(());
    };
    let mut removed = 0;
    for (_event, value) in hooks.iter_mut() {
        if let Some(arr) = value.as_array_mut() {
            let before = arr.len();
            arr.retain(|item| {
                let ours = item
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|hs| {
                        hs.iter().any(|h| {
                            h.get("command")
                                .and_then(|c| c.as_str())
                                .map(|s| s.contains(HOOK_MARK))
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false);
                !ours
            });
            removed += before - arr.len();
        }
    }
    let buf = serde_json::to_vec_pretty(&data).context("serialize settings")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &buf).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    println!("{}: removed {removed} agora hook entr(ies)", path.display());
    Ok(())
}

/// Daemon's `fs::canonicalize` is relative to the daemon's cwd (often `/`),
/// so resolve user-relative paths here before sending. Daemon still does its
/// own canonicalize (symlinks, etc) afterwards.
fn remote_add(host: &str) -> Result<()> {
    if host.is_empty() {
        bail!("host must not be empty");
    }
    let uid = ssh_query_uid(host)?;
    let payload = call(Request::RemoteAdd {
        host: host.to_string(),
        remote_uid: uid,
    })?;
    let Payload::Remote(r) = payload else {
        bail!("unexpected payload from daemon: {payload:?}");
    };
    println!(
        "added remote: {} (remote_socket={}, status={:?})",
        r.host.host, r.host.remote_socket, r.status
    );
    Ok(())
}

fn remote_remove(host: &str) -> Result<()> {
    let payload = call(Request::RemoteRemove {
        host: host.to_string(),
    })?;
    if !matches!(payload, Payload::Ack) {
        bail!("unexpected payload from daemon: {payload:?}");
    }
    println!("removed remote: {host}");
    Ok(())
}

fn remote_list(json: bool) -> Result<()> {
    let payload = call(Request::RemoteList)?;
    let Payload::Remotes(remotes) = payload else {
        bail!("unexpected payload from daemon: {payload:?}");
    };
    if json {
        serde_json::to_writer(std::io::stdout(), &remotes).context("serialize remotes")?;
        println!();
    } else if remotes.is_empty() {
        println!("(no remotes)");
    } else {
        for r in remotes {
            let status = match r.status {
                agora::ipc::RemoteStatus::Connected => "connected",
                agora::ipc::RemoteStatus::Connecting => "connecting",
                agora::ipc::RemoteStatus::Disconnected => "disconnected",
                agora::ipc::RemoteStatus::Failed => "failed",
            };
            let err = r
                .last_error
                .as_deref()
                .map(|s| format!(" ({s})"))
                .unwrap_or_default();
            println!(
                "{:20} status={status} sock={}{err}",
                r.host.host, r.host.remote_socket
            );
        }
    }
    Ok(())
}

fn ssh_query_uid(host: &str) -> Result<u32> {
    let out = Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg(host)
        .arg("id -u")
        .output()
        .with_context(|| format!("run `ssh {host} id -u`"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("ssh {host} id -u failed: {}", err.trim());
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let trimmed = s.trim();
    trimmed
        .parse::<u32>()
        .with_context(|| format!("remote returned non-numeric uid: {trimmed:?}"))
}

fn remote_install(host: &str) -> Result<()> {
    if host.is_empty() {
        bail!("host must not be empty");
    }
    // Local release binary — what we ship to the remote.
    let local_bin = local_release_binary()?;
    println!("local binary: {}", local_bin.display());

    // 1. ssh probe: arch + uid (we'll need both)
    let probe = Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", host])
        .arg("uname -m && id -u")
        .output()
        .with_context(|| format!("ssh probe to {host}"))?;
    if !probe.status.success() {
        bail!(
            "ssh probe failed: {}",
            String::from_utf8_lossy(&probe.stderr).trim()
        );
    }
    let probe_out = String::from_utf8_lossy(&probe.stdout);
    let mut lines = probe_out.lines();
    let arch = lines.next().unwrap_or("").trim().to_string();
    let uid: u32 = lines
        .next()
        .unwrap_or("")
        .trim()
        .parse()
        .with_context(|| format!("remote returned non-numeric uid: {probe_out:?}"))?;
    println!("remote: arch={arch} uid={uid}");
    if arch != "x86_64" {
        bail!(
            "remote arch '{arch}' is not x86_64; cross-arch install not yet supported"
        );
    }

    // 2. ensure remote ~/.local/bin exists
    let mk = Command::new("ssh")
        .args(["-o", "BatchMode=yes", host])
        .arg("mkdir -p ~/.local/bin")
        .status()
        .context("ssh mkdir")?;
    if !mk.success() {
        bail!("remote mkdir ~/.local/bin failed");
    }

    // 3. scp the binary
    println!("scp -> {host}:~/.local/bin/agora");
    let scp = Command::new("scp")
        .args(["-o", "BatchMode=yes"])
        .arg(&local_bin)
        .arg(format!("{host}:.local/bin/agora"))
        .status()
        .context("scp binary")?;
    if !scp.success() {
        bail!("scp failed");
    }

    // 4. chmod + sanity-check version
    let verify = Command::new("ssh")
        .args(["-o", "BatchMode=yes", host])
        .arg("chmod +x ~/.local/bin/agora && ~/.local/bin/agora --version")
        .output()
        .context("ssh verify")?;
    if !verify.status.success() {
        bail!(
            "remote agora --version failed: {}",
            String::from_utf8_lossy(&verify.stderr).trim()
        );
    }
    println!(
        "remote agora: {}",
        String::from_utf8_lossy(&verify.stdout).trim()
    );

    // 5. install claude hooks on remote (with host alias so agents report SSH alias)
    let hooks = Command::new("ssh")
        .args(["-o", "BatchMode=yes", host])
        .arg(format!(
            "~/.local/bin/agora hook install --host-alias {host}"
        ))
        .output()
        .context("ssh hook install")?;
    if !hooks.status.success() {
        bail!(
            "remote hook install failed: {}",
            String::from_utf8_lossy(&hooks.stderr).trim()
        );
    }
    println!("hook install: {}", String::from_utf8_lossy(&hooks.stdout).trim());

    // 6. register the host with the local daemon (idempotent: re-add updates)
    let payload = call(Request::RemoteAdd {
        host: host.to_string(),
        remote_uid: uid,
    })?;
    let Payload::Remote(r) = payload else {
        bail!("unexpected payload from daemon: {payload:?}");
    };
    println!(
        "registered: {} (remote_socket={}, status={:?})",
        r.host.host, r.host.remote_socket, r.status
    );
    Ok(())
}

fn local_release_binary() -> Result<std::path::PathBuf> {
    // Search from the current cwd upward for target/release/agora.
    // Defaults to whichever cwd we're invoked from (likely the user's project).
    let exe = std::env::current_exe().context("read current_exe")?;
    // exe is at .../target/release/agora — we're already that.
    if exe.ends_with("target/release/agora") || exe.ends_with("target/x86_64-unknown-linux-gnu/release/agora") {
        return Ok(exe);
    }
    // Try ~/.cargo or PATH-resolved binary's path. As fallback look at $AGORA_BIN.
    if let Ok(p) = std::env::var("AGORA_BIN") {
        let pb = std::path::PathBuf::from(p);
        if pb.exists() {
            return Ok(pb);
        }
    }
    // Last resort: which agora
    let out = Command::new("which").arg("agora").output().ok();
    if let Some(o) = out {
        if o.status.success() {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !s.is_empty() {
                return Ok(std::path::PathBuf::from(s));
            }
        }
    }
    bail!("could not locate agora release binary; set AGORA_BIN=/path/to/agora")
}

fn resolve_local_path(path: &str) -> Result<String> {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        return Ok(path.to_string());
    }
    let cwd = std::env::current_dir().context("read cwd")?;
    Ok(cwd.join(p).to_string_lossy().into_owned())
}

fn parse_launcher(s: &str) -> Result<Launcher, String> {
    match s {
        "vscode" => Ok(Launcher::Vscode),
        "zed" => Ok(Launcher::Zed),
        "kitty" => Ok(Launcher::Kitty { run: None }),
        "claude" => Ok(Launcher::Claude),
        "codex" => Ok(Launcher::Codex),
        other => {
            if let Some(cmd) = other.strip_prefix("kitty:") {
                Ok(Launcher::Kitty {
                    run: Some(cmd.to_string()),
                })
            } else {
                Err(format!(
                    "unknown launcher '{other}' \
                     (expected: vscode|zed|kitty|kitty:CMD|claude|codex)"
                ))
            }
        }
    }
}

fn call(req: Request) -> Result<Payload> {
    let path = ipc::socket_path()?;
    let stream = UnixStream::connect(&path)
        .with_context(|| format!("connect to agorad at {}", path.display()))?;
    let mut reader = BufReader::new(stream.try_clone().context("clone stream")?);
    let mut writer = stream;
    ipc::write_line(&mut writer, &req)?;
    let resp: Response = ipc::read_line(&mut reader)?;
    resp.into_result()
}
