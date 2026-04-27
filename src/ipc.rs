//! Wire protocol between `agora` (CLI) and `agorad` (daemon).
//!
//! JSON-line protocol over a unix socket: client writes one `Request` JSON
//! per line, daemon writes one `Response` JSON per line. Connection closes
//! after each round-trip — no multi-request sessions in MVP.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::Project;

pub fn socket_path() -> Result<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR not set")?;
    if dir.is_empty() {
        anyhow::bail!("XDG_RUNTIME_DIR is empty");
    }
    Ok(PathBuf::from(dir).join("agora.sock"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Add { name: String, root_path: String },
    List,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Ok(Payload),
    Err(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Payload {
    Project(Project),
    Projects(Vec<Project>),
}

impl Response {
    pub fn into_result(self) -> Result<Payload> {
        match self {
            Response::Ok(p) => Ok(p),
            Response::Err(msg) => Err(anyhow::anyhow!("daemon: {msg}")),
        }
    }
}

pub fn write_line<W: std::io::Write, T: Serialize>(w: &mut W, value: &T) -> Result<()> {
    let mut buf = serde_json::to_vec(value).context("serialize")?;
    buf.push(b'\n');
    w.write_all(&buf).context("write line")?;
    w.flush().context("flush")?;
    Ok(())
}

pub fn read_line<R: std::io::BufRead, T: for<'de> Deserialize<'de>>(r: &mut R) -> Result<T> {
    let mut buf = String::new();
    let n = r.read_line(&mut buf).context("read line")?;
    if n == 0 {
        anyhow::bail!("connection closed");
    }
    serde_json::from_str(buf.trim_end()).context("parse line")
}
