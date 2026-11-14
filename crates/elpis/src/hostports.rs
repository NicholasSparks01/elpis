//! Host ports without a network stack: a Windows AF_UNIX socket per port, relayed to 127.0.0.1:PORT.
//! The socket's folder is shared into the container; Sailor's guest kernel redirects `connect()` on the
//! shared socket file to it ("uds-forward").

use crate::log::{Color, Log};
use crate::unixsock::{self, Listener};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use socket2::Socket;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

pub fn serve(socket_path: &Path, port: u16, log: &Log) -> Result<Listener> {
    let count = Arc::new(AtomicU32::new(0));
    let listener = {
        let log = log.clone();
        unixsock::listen(socket_path, move |guest| {
            let (log, n) = (log.clone(), count.fetch_add(1, Ordering::SeqCst) + 1);
            std::thread::spawn(move || relay(guest, port, n, &log));
        })?
    };
    let name = socket_path.file_name().unwrap_or_default().to_string_lossy();
    log.line("port", Color::Blue, &format!("{name} -> 127.0.0.1:{port}"));
    Ok(listener)
}

fn relay(guest: Socket, port: u16, n: u32, log: &Log) {
    let host = match TcpStream::connect(("127.0.0.1", port)) {
        Ok(host) => host,
        Err(e) => {
            log.line("port", Color::Yellow, &format!("{port} #{n}: can't reach 127.0.0.1:{port}: {e}"));
            return;
        }
    };
    let (up, down) = std::thread::scope(|s| {
        let up = s.spawn(|| pump(&guest, &host, || host.shutdown(Shutdown::Write)));
        let down = s.spawn(|| pump(&host, &guest, || guest.shutdown(Shutdown::Write)));
        (up.join().unwrap_or(0), down.join().unwrap_or(0))
    });
    log.line("port", Color::Blue, &format!("{port} #{n}: closed, {up} bytes up, {down} bytes down"));
}

/// Copies until the reader ends, then half-closes the other side so it sees end-of-stream.
fn pump(mut from: impl Read, mut to: impl Write, shutdown: impl FnOnce() -> std::io::Result<()>) -> u64 {
    let mut buf = [0u8; 65536];
    let mut total = 0u64;
    loop {
        match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
                total += n as u64;
            }
        }
    }
    let _ = shutdown();
    total
}

/// Adds (or replaces) a policybind sharing `host_dir` at `target`, in the same form sbx uses.
pub fn share_policy_bind(config_path: &Path, host_dir: &Path, target: &str) -> Result<()> {
    let mut spec: Value = serde_json::from_str(&std::fs::read_to_string(config_path)?)?;
    let annotations = spec["annotations"].as_object_mut().context("spec has no annotations")?;
    let mut binds: Vec<Value> = annotations
        .get("io.containerd.nerdbox.policybinds")
        .and_then(Value::as_str)
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_default();
    binds.retain(|b| b["target"] != target);
    let full = std::path::absolute(host_dir)?.to_string_lossy().into_owned();
    // C:\x\y -> /c/x/y
    let guest_subpath = format!("/{}{}", full[..1].to_lowercase(), full[2..].replace('\\', "/"));
    binds.push(json!({ "source": full, "guestSubpath": guest_subpath, "target": target, "readOnly": false }));
    annotations.insert("io.containerd.nerdbox.policybinds".into(), Value::String(serde_json::to_string(&binds)?));
    std::fs::write(config_path, serde_json::to_string_pretty(&spec)?)?;
    Ok(())
}

/// For elpis-shim: each port's socket as an OCI `uds` mount (containerd/nerdbox socket forwarding) at
/// `<target>/tcp-PORT.sock`, owned by the container's user so it can connect. Replaces earlier ones (and the
/// sbx-style policybind of the same folder); with no ports, just removes them.
pub fn set_uds_mounts(config_path: &Path, host_dir: &Path, ports: &[u16], target: &str) -> Result<()> {
    let mut spec: Value = serde_json::from_str(&std::fs::read_to_string(config_path)?)?;
    let (uid, gid) = (spec["process"]["user"]["uid"].as_u64().unwrap_or(0), spec["process"]["user"]["gid"].as_u64().unwrap_or(0));
    let prefix = format!("{target}/");
    let mounts = spec["mounts"].as_array_mut().context("spec has no mounts")?;
    let before = mounts.len();
    mounts.retain(|m| !(m["type"] == "uds" && m["destination"].as_str().is_some_and(|d| d.starts_with(&prefix))));
    let mut changed = mounts.len() != before;
    let full = std::path::absolute(host_dir)?;
    for port in ports {
        mounts.push(json!({
            "destination": format!("{target}/tcp-{port}.sock"),
            "type": "uds",
            "source": full.join(format!("tcp-{port}.sock")).to_string_lossy(),
            "options": [format!("uid={uid}"), format!("gid={gid}")],
        }));
        changed = true;
    }
    if let Some(annotations) = spec["annotations"].as_object_mut()
        && let Some(binds) = annotations.get("io.containerd.nerdbox.policybinds").and_then(Value::as_str)
    {
        let mut binds: Vec<Value> = serde_json::from_str(binds)?;
        let n = binds.len();
        binds.retain(|b| b["target"] != target);
        if binds.len() != n {
            annotations.insert("io.containerd.nerdbox.policybinds".into(), Value::String(serde_json::to_string(&binds)?));
            changed = true;
        }
    }
    if changed {
        std::fs::write(config_path, serde_json::to_string_pretty(&spec)?)?;
    }
    Ok(())
}
