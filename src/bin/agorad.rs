//! agorad — niri workspace manager daemon
//!
//! Subscribes to niri events, manages project state, exposes a unix socket.
//! See VISION.html §12 milestones.

use anyhow::{Context, Result, bail};
use niri_ipc::socket::Socket;
use niri_ipc::{Event, Reply, Request, Response};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    tracing::info!("agorad v{} starting", env!("CARGO_PKG_VERSION"));

    let mut socket = Socket::connect().context("connecting to niri IPC socket")?;
    let reply: Reply = socket
        .send(Request::EventStream)
        .context("requesting event stream")?;

    match reply {
        Ok(Response::Handled) => {}
        Ok(other) => bail!("unexpected response from niri: {:?}", other),
        Err(msg) => bail!("niri rejected event stream request: {}", msg),
    }

    tracing::info!("subscribed to niri event stream");
    let mut read_event = socket.read_events();

    loop {
        let event = read_event().context("reading niri event")?;
        log_event(&event);
    }
}

fn log_event(event: &Event) {
    match serde_json::to_string(event) {
        Ok(json) => tracing::info!(target: "niri", "{json}"),
        Err(e) => tracing::warn!(target: "niri", error = %e, "serialize failed"),
    }
}
