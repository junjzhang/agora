//! nwmd — niri workspace manager daemon
//!
//! Subscribes to niri events, manages project state, exposes a unix socket.
//! See VISION.html §12 milestones.

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    tracing::info!("nwmd v{} (stub)", env!("CARGO_PKG_VERSION"));
    tracing::warn!("not implemented yet");
    Ok(())
}
