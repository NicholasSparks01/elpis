//! containerd creates the container's stdio pipes and the shim connects to them.
//! They must be two-way: the shim's Go pipe client opens with read and write access,
//! which Windows refuses on an inbound-only pipe ("Access is denied").

use crate::log::{Color, Log};
use std::io;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::task::JoinHandle;

pub fn path(name: &str) -> String {
    format!(r"\\.\pipe\{name}")
}

/// A two-way pipe with default security, created now so it exists before the shim is told about it.
pub fn create_pipe(name: &str) -> io::Result<NamedPipeServer> {
    ServerOptions::new().first_pipe_instance(true).create(path(name))
}

/// Logs each line written to the pipe; the task ends when the writer closes it.
pub fn serve(name: &str, tag: String, color: Color, log: &Log) -> io::Result<JoinHandle<()>> {
    let pipe = create_pipe(name)?;
    let log = log.clone();
    Ok(tokio::spawn(async move {
        if pipe.connect().await.is_err() {
            return;
        }
        let mut lines = BufReader::new(pipe).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            log.line(&tag, color, &line);
        }
    }))
}
