//! SSH reverse-forward tunnel lifecycle for remote agent hosts.
//!
//! Each registered remote has a tunnel thread that keeps an `ssh -N -R` alive
//! and re-dials with exponential backoff on failure. State (status, last
//! error, shutdown channel) lives in the corresponding `RemoteBackend` in
//! `Inner.remotes`.

use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use agora::ipc::{self, RemoteStatus, RemoteSummary};
use agora::model::RemoteHost;
use agora::store;

use crate::backend::RemoteBackend;
use crate::State;

pub(crate) fn start_tunnel(state: &State, host: RemoteHost) {
    // Stop any prior tunnel for this host.
    {
        let mut inner = state.lock().unwrap();
        if let Some(remote) = inner.remotes.get_mut(&host.host) {
            if let Some(tx) = remote.shutdown_tx.take() {
                let _ = tx.send(());
            }
        }
    }

    let (tx, rx) = std::sync::mpsc::channel::<()>();
    {
        let mut inner = state.lock().unwrap();
        let remote = inner
            .remotes
            .entry(host.host.clone())
            .or_insert_with(|| RemoteBackend::new(host.clone()));
        remote.config = host.clone();
        remote.status = RemoteStatus::Connecting;
        remote.last_error = None;
        remote.shutdown_tx = Some(tx);
    }

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
    if let Some(remote) = inner.remotes.get_mut(host) {
        remote.status = status;
        remote.last_error = last_error;
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
        let inner = state.lock().unwrap();
        let persisted: Vec<RemoteHost> = inner
            .remotes
            .values()
            .filter(|r| !r.is_ephemeral())
            .map(|r| r.config.clone())
            .filter(|r| r.host != host_name)
            .chain(std::iter::once(new_host.clone()))
            .collect();
        store::save_remotes(&persisted).context("save remotes store")?;
    }
    start_tunnel(state, new_host.clone());
    Ok(remote_summary(state, &host_name))
}

pub(crate) fn remote_remove(state: &State, host_name: String) -> Result<()> {
    let shutdown_tx = {
        let mut inner = state.lock().unwrap();
        // Refuse to remove an ephemeral remote (auto-created for a hook
        // from an unregistered host). It's not user-managed and removing
        // it would drop active in-memory sessions on that host.
        match inner.remotes.get(&host_name) {
            None => bail!("no remote named '{host_name}'"),
            Some(r) if r.is_ephemeral() => {
                bail!("'{host_name}' is not a configured remote; nothing to remove")
            }
            _ => {}
        }
        let mut remote = inner.remotes.remove(&host_name).unwrap();
        let persisted: Vec<RemoteHost> = inner
            .remotes
            .values()
            .filter(|r| !r.is_ephemeral())
            .map(|r| r.config.clone())
            .collect();
        store::save_remotes(&persisted).context("save remotes store")?;
        remote.shutdown_tx.take()
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
        .values()
        .filter(|r| !r.is_ephemeral())
        .map(|r| RemoteSummary {
            host: r.config.clone(),
            status: r.status,
            last_error: r.last_error.clone(),
        })
        .collect()
}

fn remote_summary(state: &State, host_name: &str) -> RemoteSummary {
    let inner = state.lock().unwrap();
    let remote = inner.remotes.get(host_name);
    match remote {
        Some(r) => RemoteSummary {
            host: r.config.clone(),
            status: r.status,
            last_error: r.last_error.clone(),
        },
        None => RemoteSummary {
            host: RemoteHost {
                host: host_name.to_string(),
                remote_socket: String::new(),
                remote_agora_path: None,
                auto_connect: false,
            },
            status: RemoteStatus::Disconnected,
            last_error: None,
        },
    }
}
