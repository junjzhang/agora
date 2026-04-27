//! agora — niri workspace manager CLI
//!
//! Thin client to agorad. Each subcommand maps to one IPC round-trip.
//! See VISION.html §8 for the full command surface.

use std::io::BufReader;
use std::os::unix::net::UnixStream;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use agora::ipc::{self, Payload, Request, Response};

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
    },
    /// List all projects
    List,
    /// Focus the project's niri workspace; claim the current workspace if not yet named
    Open {
        /// Project name
        name: String,
    },
    /// Show daemon state — projects + currently-claimed windows
    Status,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Add { name, path } => {
            let payload = call(Request::Add {
                name,
                root_path: path,
            })?;
            let Payload::Project(p) = payload else {
                anyhow::bail!("unexpected payload from daemon: {payload:?}");
            };
            println!("added: {} -> {}", p.id, p.roots[0].path);
        }
        Cmd::List => {
            let payload = call(Request::List)?;
            let Payload::Projects(projects) = payload else {
                anyhow::bail!("unexpected payload from daemon: {payload:?}");
            };
            if projects.is_empty() {
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
