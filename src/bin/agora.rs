//! agora — niri workspace manager CLI
//!
//! Thin client to agorad. Each subcommand maps to one IPC round-trip.
//! See VISION.html §8 for the full command surface.

use std::io::BufReader;
use std::os::unix::net::UnixStream;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use agora::ipc::{self, Payload, Request, Response};
use agora::model::Launcher;

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
        /// Root directory
        #[arg(default_value = ".")]
        path: String,
        /// Add a launcher to spawn on `agora open`. May be repeated.
        ///
        /// Format: `vscode` | `zed` | `kitty` | `kitty:CMD`
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
    /// Manage roots within a project
    Root {
        #[command(subcommand)]
        action: RootCmd,
    },
}

#[derive(Subcommand)]
enum RootCmd {
    /// Add another root directory to an existing project
    Add {
        /// Project name
        project: String,
        /// Root directory
        path: String,
        /// Optional label for this root (e.g. "code", "paper", "data")
        #[arg(long)]
        label: Option<String>,
        /// Add a launcher tied to this root. May be repeated.
        ///
        /// Format: `vscode` | `zed` | `kitty` | `kitty:CMD`
        #[arg(long = "launcher", value_name = "KIND", value_parser = parse_launcher)]
        launchers: Vec<Launcher>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Add {
            name,
            path,
            launchers,
        } => {
            let payload = call(Request::Add {
                name,
                root_path: path,
                launchers,
            })?;
            let Payload::Project(p) = payload else {
                anyhow::bail!("unexpected payload from daemon: {payload:?}");
            };
            let kinds: Vec<&'static str> = p.roots[0].launchers.iter().map(|l| l.kind()).collect();
            if kinds.is_empty() {
                println!("added: {} -> {}", p.id, p.roots[0].path);
            } else {
                println!(
                    "added: {} -> {} (launchers: {})",
                    p.id,
                    p.roots[0].path,
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
        Cmd::Root {
            action:
                RootCmd::Add {
                    project,
                    path,
                    label,
                    launchers,
                },
        } => {
            let payload = call(Request::RootAdd {
                project,
                path,
                label,
                launchers,
            })?;
            let Payload::Project(p) = payload else {
                anyhow::bail!("unexpected payload from daemon: {payload:?}");
            };
            let added = p.roots.last().expect("RootAdd must produce a root");
            let label = added.label.as_deref().unwrap_or("-");
            let kinds: Vec<&'static str> = added.launchers.iter().map(|l| l.kind()).collect();
            if kinds.is_empty() {
                println!("root added: {} <- {} (label={})", p.id, added.path, label);
            } else {
                println!(
                    "root added: {} <- {} (label={}, launchers: {})",
                    p.id,
                    added.path,
                    label,
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
                    let ws = w
                        .workspace_id
                        .map(|i| i.to_string())
                        .unwrap_or_else(|| "-".into());
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

fn parse_launcher(s: &str) -> Result<Launcher, String> {
    match s {
        "vscode" => Ok(Launcher::Vscode),
        "zed" => Ok(Launcher::Zed),
        "kitty" => Ok(Launcher::Kitty { run: None }),
        other => {
            if let Some(cmd) = other.strip_prefix("kitty:") {
                Ok(Launcher::Kitty {
                    run: Some(cmd.to_string()),
                })
            } else {
                Err(format!(
                    "unknown launcher '{other}' (expected: vscode|zed|kitty|kitty:CMD)"
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
