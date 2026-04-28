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

/// Daemon's `fs::canonicalize` is relative to the daemon's cwd (often `/`),
/// so resolve user-relative paths here before sending. Daemon still does its
/// own canonicalize (symlinks, etc) afterwards.
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
