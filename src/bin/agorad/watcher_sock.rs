//! Watcher socket — handles `WatcherRequest::Watch/Forget` from hook
//! scripts on the same host. Hook scripts post to this socket after
//! delivering the regular hook event to the daemon, so the liveness
//! subsystem can pidfd-watch the agent's PID.
//!
//! Lives at `XDG_RUNTIME_DIR/agora-watcher.sock`. Distinct from
//! `agora.sock` because the latter is SSH-forwarded to the daemon's host
//! while watcher commands always stay on the host where the PID lives.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};

use agora::ipc::{self, WatcherRequest, WatcherResponse};

use crate::liveness::Liveness;

/// Session metadata kept by watcher-only mode so we know how to synthesize
/// the SessionEnd hook when a PID dies. Full mode doesn't need this — the
/// daemon already has the metadata in its agents map.
#[derive(Debug, Clone)]
pub(crate) struct WatchEntry {
    pub cli: String,
    pub host: Option<String>,
}

pub(crate) type WatchMap = Arc<Mutex<HashMap<String, WatchEntry>>>;

pub(crate) fn serve(listener: UnixListener, liveness: Liveness, watches: WatchMap) {
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let liveness = liveness.clone();
                let watches = watches.clone();
                std::thread::spawn(move || {
                    if let Err(e) = handle_client(stream, liveness, watches) {
                        tracing::warn!(error = %e, "watcher client handler failed");
                    }
                });
            }
            Err(e) => tracing::warn!(error = %e, "watcher accept failed"),
        }
    }
}

fn handle_client(stream: UnixStream, liveness: Liveness, watches: WatchMap) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone().context("clone stream")?);
    let mut writer = stream;
    let req: WatcherRequest = ipc::read_line(&mut reader)?;
    match req {
        WatcherRequest::Watch {
            session_id,
            pid,
            cli,
            host,
        } => {
            watches
                .lock()
                .unwrap()
                .insert(session_id.clone(), WatchEntry { cli, host });
            liveness.watch(session_id, pid);
        }
        WatcherRequest::Forget { session_id } => {
            watches.lock().unwrap().remove(&session_id);
            liveness.forget(session_id);
        }
    }
    ipc::write_line(&mut writer, &WatcherResponse::Ok)?;
    Ok(())
}

pub(crate) fn bind_socket(path: &Path) -> Result<UnixListener> {
    match UnixListener::bind(path) {
        Ok(l) => Ok(l),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => match UnixStream::connect(path) {
            Ok(_) => bail!(
                "another agora-watcher is already running on {}",
                path.display()
            ),
            Err(_) => {
                fs::remove_file(path)
                    .with_context(|| format!("remove stale {}", path.display()))?;
                UnixListener::bind(path).with_context(|| format!("rebind {}", path.display()))
            }
        },
        Err(e) => Err(e).with_context(|| format!("bind {}", path.display())),
    }
}
