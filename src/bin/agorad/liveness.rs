//! pidfd-based liveness watcher.
//!
//! Subscribes to PID exits via the kernel's `pidfd_open` syscall. A single
//! tokio reactor multiplexes hundreds of pidfds via one shared epoll fd —
//! O(1) memory per watched session and zero CPU while waiting.
//!
//! When a watched PID exits, the session id is sent to `exit_rx` so the
//! caller can deliver a synthetic SessionEnd through whatever channel makes
//! sense (in-process backend pruning for full daemon mode, or shelling out
//! to `agora hook event` for watcher-only mode).

use std::collections::HashMap;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::mpsc;

use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc as tokio_mpsc;

/// Commands the daemon (or watcher socket listener) sends to the liveness
/// thread.
#[derive(Debug)]
pub(crate) enum LivenessCmd {
    Watch { session_id: String, pid: i32 },
    Forget { session_id: String },
}

/// Handle to the liveness subsystem. Sending side of the command channel;
/// `Clone` so it can be shared across handler threads.
#[derive(Clone)]
pub(crate) struct Liveness {
    tx: tokio_mpsc::UnboundedSender<LivenessCmd>,
}

impl Liveness {
    pub fn watch(&self, session_id: String, pid: i32) {
        let _ = self.tx.send(LivenessCmd::Watch { session_id, pid });
    }

    pub fn forget(&self, session_id: String) {
        let _ = self.tx.send(LivenessCmd::Forget { session_id });
    }
}

/// Spawn the liveness subsystem on a dedicated thread with its own tokio
/// runtime. Returns the handle plus a sync receiver that emits a
/// `session_id` whenever a watched PID exits.
pub(crate) fn spawn() -> (Liveness, mpsc::Receiver<String>) {
    let (cmd_tx, mut cmd_rx) = tokio_mpsc::unbounded_channel::<LivenessCmd>();
    let (exit_tx, exit_rx) = mpsc::channel::<String>();

    std::thread::Builder::new()
        .name("liveness".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!(error = %e, "build tokio runtime for liveness");
                    return;
                }
            };
            rt.block_on(async move {
                let mut watches: HashMap<String, tokio::task::AbortHandle> = HashMap::new();
                while let Some(cmd) = cmd_rx.recv().await {
                    match cmd {
                        LivenessCmd::Watch { session_id, pid } => {
                            let sid = session_id.clone();
                            let exit_tx = exit_tx.clone();
                            let join = tokio::spawn(async move {
                                match watch_pid(pid).await {
                                    Ok(()) => {
                                        let _ = exit_tx.send(sid);
                                    }
                                    Err(e) => {
                                        // Could not open pidfd (PID already gone,
                                        // permission denied, etc.). Still synthesize
                                        // an exit — the session is effectively dead
                                        // from our point of view.
                                        tracing::warn!(
                                            pid,
                                            error = %e,
                                            "pidfd watch failed; emitting exit"
                                        );
                                        let _ = exit_tx.send(sid);
                                    }
                                }
                            });
                            let abort = join.abort_handle();
                            if let Some(prev) = watches.insert(session_id, abort) {
                                prev.abort();
                            }
                        }
                        LivenessCmd::Forget { session_id } => {
                            if let Some(abort) = watches.remove(&session_id) {
                                abort.abort();
                            }
                        }
                    }
                }
            });
        })
        .expect("spawn liveness thread");

    (Liveness { tx: cmd_tx }, exit_rx)
}

/// Block until `pid` exits. Returns Ok once the kernel reports exit;
/// Err if the pidfd could not be opened or polled.
async fn watch_pid(pid: i32) -> std::io::Result<()> {
    // pidfd_open(pid, flags=0). Returns -1 on error.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
    let async_fd = AsyncFd::new(fd)?;
    // A pidfd becomes readable exactly once — when the process exits.
    let _guard = async_fd.readable().await?;
    Ok(())
}
