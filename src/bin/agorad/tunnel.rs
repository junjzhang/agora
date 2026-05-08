use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use agora::ipc::{self, RemoteStatus, RemoteSummary};
use agora::model::RemoteHost;
use agora::store;

use crate::State;

#[derive(Debug, Clone)]
pub(crate) struct TunnelState {
    pub status: RemoteStatus,
    pub last_error: Option<String>,
    pub shutdown_tx: Option<std::sync::mpsc::Sender<()>>,
}

pub(crate) fn start_tunnel(state: &State, host: RemoteHost) {
    let prev = state.lock().unwrap().tunnels.remove(&host.host);
    if let Some(t) = prev {
        if let Some(tx) = t.shutdown_tx {
            let _ = tx.send(());
        }
    }

    let (tx, rx) = std::sync::mpsc::channel::<()>();
    state.lock().unwrap().tunnels.insert(
        host.host.clone(),
        TunnelState {
            status: RemoteStatus::Connecting,
            last_error: None,
            shutdown_tx: Some(tx),
        },
    );

    let state = state.clone();
    let host_clone = host.clone();
    std::thread::Builder::new()
        .name(format!("ssh-{}", host.host))
        .spawn(move || tunnel_loop(state, host_clone, rx))
        .expect("spawn ssh tunnel thread");
}

fn tunnel_loop(state: State, host: RemoteHost, shutdown_rx: std::sync::mpsc::Receiver<()>) {
    let local_socket = match ipc::socket_path() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => {
            tracing::error!(host = %host.host, error = %e, "cannot resolve local socket");
            set_tunnel_status(
                &state,
                &host.host,
                RemoteStatus::Failed,
                Some(e.to_string()),
            );
            return;
        }
    };

    let mut backoff_secs: u64 = 1;
    loop {
        if shutdown_rx.try_recv().is_ok() {
            tracing::info!(host = %host.host, "tunnel shutting down");
            set_tunnel_status(&state, &host.host, RemoteStatus::Disconnected, None);
            return;
        }

        let args = build_ssh_args(&host, &local_socket);
        tracing::info!(host = %host.host, ?args, "starting ssh tunnel");
        set_tunnel_status(&state, &host.host, RemoteStatus::Connecting, None);

        let mut child = match Command::new("ssh")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(host = %host.host, error = %e, "ssh spawn failed");
                set_tunnel_status(
                    &state,
                    &host.host,
                    RemoteStatus::Failed,
                    Some(format!("spawn: {e}")),
                );
                if !sleep_or_shutdown(&shutdown_rx, backoff_secs) {
                    return;
                }
                backoff_secs = (backoff_secs * 2).min(60);
                continue;
            }
        };

        let stderr = child.stderr.take();
        let host_for_log = host.host.clone();
        let stderr_thread = stderr.map(|s| {
            std::thread::spawn(move || {
                use std::io::{BufRead, BufReader};
                let mut last = String::new();
                for line in BufReader::new(s).lines().map_while(|l| l.ok()) {
                    tracing::debug!(host = %host_for_log, "ssh stderr: {line}");
                    last = line;
                }
                last
            })
        });

        // Optimistic: assume connected after 1.5s of running ssh.
        std::thread::sleep(std::time::Duration::from_millis(1500));
        if child.try_wait().ok().flatten().is_none() {
            set_tunnel_status(&state, &host.host, RemoteStatus::Connected, None);
            backoff_secs = 1;
        }

        let exit = child.wait();
        let last_stderr = stderr_thread
            .and_then(|h| h.join().ok())
            .filter(|s| !s.is_empty());

        if shutdown_rx.try_recv().is_ok() {
            let _ = child.kill();
            set_tunnel_status(&state, &host.host, RemoteStatus::Disconnected, None);
            return;
        }

        let err_summary = match exit {
            Ok(s) => format!("ssh exited: {s}"),
            Err(e) => format!("wait failed: {e}"),
        };
        let display_err = last_stderr.clone().unwrap_or_else(|| err_summary.clone());
        tracing::warn!(host = %host.host, "{err_summary}; stderr={:?}", last_stderr);
        set_tunnel_status(&state, &host.host, RemoteStatus::Failed, Some(display_err));

        if !sleep_or_shutdown(&shutdown_rx, backoff_secs) {
            return;
        }
        backoff_secs = (backoff_secs * 2).min(60);
    }
}

fn build_ssh_args(host: &RemoteHost, local_socket: &str) -> Vec<String> {
    vec![
        "-N".into(),
        "-T".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ServerAliveInterval=10".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
        "-o".into(),
        "StreamLocalBindUnlink=yes".into(),
        "-o".into(),
        "StreamLocalBindMask=0077".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-o".into(),
        "ControlMaster=no".into(),
        "-R".into(),
        format!("{}:{}", host.remote_socket, local_socket),
        host.host.clone(),
    ]
}

fn set_tunnel_status(state: &State, host: &str, status: RemoteStatus, last_error: Option<String>) {
    let mut inner = state.lock().unwrap();
    if let Some(t) = inner.tunnels.get_mut(host) {
        t.status = status;
        t.last_error = last_error;
    }
}

fn sleep_or_shutdown(rx: &std::sync::mpsc::Receiver<()>, secs: u64) -> bool {
    match rx.recv_timeout(std::time::Duration::from_secs(secs)) {
        Ok(()) => false,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => true,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => false,
    }
}

pub(crate) fn remote_add(
    state: &State,
    host_name: String,
    remote_uid: u32,
) -> Result<RemoteSummary> {
    if host_name.is_empty() {
        bail!("remote host must not be empty");
    }
    let new_host = RemoteHost {
        host: host_name.clone(),
        remote_socket: format!("/run/user/{remote_uid}/agora.sock"),
        remote_agora_path: None,
        auto_connect: true,
    };
    {
        let mut inner = state.lock().unwrap();
        if let Some(existing) = inner.remotes.iter_mut().find(|r| r.host == host_name) {
            *existing = new_host.clone();
        } else {
            inner.remotes.push(new_host.clone());
        }
        store::save_remotes(&inner.remotes).context("save remotes store")?;
    }
    start_tunnel(state, new_host.clone());
    Ok(remote_summary(state, &host_name))
}

pub(crate) fn remote_remove(state: &State, host_name: String) -> Result<()> {
    let shutdown_tx = {
        let mut inner = state.lock().unwrap();
        let idx = inner
            .remotes
            .iter()
            .position(|r| r.host == host_name)
            .ok_or_else(|| anyhow::anyhow!("no remote named '{host_name}'"))?;
        inner.remotes.remove(idx);
        store::save_remotes(&inner.remotes).context("save remotes store")?;
        inner.tunnels.remove(&host_name).and_then(|t| t.shutdown_tx)
    };
    if let Some(tx) = shutdown_tx {
        let _ = tx.send(());
    }
    Ok(())
}

pub(crate) fn remote_list(state: &State) -> Vec<RemoteSummary> {
    let inner = state.lock().unwrap();
    inner
        .remotes
        .iter()
        .map(|r| {
            let (status, last_error) = inner
                .tunnels
                .get(&r.host)
                .map(|t| (t.status, t.last_error.clone()))
                .unwrap_or((RemoteStatus::Disconnected, None));
            RemoteSummary {
                host: r.clone(),
                status,
                last_error,
            }
        })
        .collect()
}

fn remote_summary(state: &State, host_name: &str) -> RemoteSummary {
    let inner = state.lock().unwrap();
    let host = inner
        .remotes
        .iter()
        .find(|r| r.host == host_name)
        .cloned()
        .unwrap_or_else(|| RemoteHost {
            host: host_name.to_string(),
            remote_socket: String::new(),
            remote_agora_path: None,
            auto_connect: false,
        });
    let (status, last_error) = inner
        .tunnels
        .get(host_name)
        .map(|t| (t.status, t.last_error.clone()))
        .unwrap_or((RemoteStatus::Disconnected, None));
    RemoteSummary {
        host,
        status,
        last_error,
    }
}
