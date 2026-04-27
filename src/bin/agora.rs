//! agora — niri workspace manager CLI
//!
//! Thin client to agorad. See VISION.html §8 for the command surface.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "agora",
    version,
    about = "agent-era niri workspace manager",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Add a project (stub)
    Add,
    /// List projects (stub)
    List,
    /// Open a project (stub)
    Open,
    /// Forget a project from MRU (stub)
    Forget,
    /// Manage roots within a project (stub)
    Root,
    /// Rename a project (stub)
    Rename,
    /// Show daemon / project status (stub)
    Status,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(c) => println!("{:?}: TODO", std::any::type_name_of_val(&c)),
        None => println!("agora v{} — run with --help", env!("CARGO_PKG_VERSION")),
    }
    Ok(())
}
