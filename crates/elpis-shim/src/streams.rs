//! Container stdio: the engine hands us named pipes (`\\.\pipe\…`); vminitd wants `stream://ID` URIs, each a
//! vsock connection to port 1026 opened with containerd/nerdbox's StartStream handshake. We open one stream per pipe and
//! copy bytes between them on plain threads. Stdin's EOF (the engine closing its pipe) becomes a half-close of
//! the stream, which vminitd sees as EOF.

use anyhow::{Context, Result, bail};
use socket2::Socket;
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

/// Dials the stream port's socket, sends `[u32 BE len][id]`, and expects the same id back.
pub fn start_stream(stream_sock: &Path, id: &str) -> Result<Socket> {
    let clock = Instant::now();
    let mut s = loop {
        match elpis::unixsock::connect(stream_sock) {
            Ok(s) => break s,
            Err(_) if clock.elapsed() < Duration::from_secs(1) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(e),
        }
    };
    s.write_all(&(id.len() as u32).to_be_bytes())?;
    s.write_all(id.as_bytes())?;
    let mut len = [0u8; 4];
    s.read_exact(&mut len)?;
    let mut ack = vec![0u8; u32::from_be_bytes(len) as usize];
    s.read_exact(&mut ack)?;
    if ack != id.as_bytes() {
        bail!("stream {id}: unexpected ack {:?}", String::from_utf8_lossy(&ack));
    }
    Ok(s)
}

/// Opens the engine's pipe as a client (two-way, as its server expects), retrying briefly while it's busy.
fn open_pipe(path: &str) -> Result<std::fs::File> {
    let clock = Instant::now();
    loop {
        match std::fs::OpenOptions::new().read(true).write(true).open(path) {
            Ok(f) => return Ok(f),
            Err(_) if clock.elapsed() < Duration::from_secs(10) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(e).with_context(|| format!("opening {path}")),
        }
    }
}

fn stream_id(prefix: &str, what: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{prefix}-{what}-{nanos}-{}", N.fetch_add(1, Ordering::Relaxed))
}

/// The stdio a Create or Exec request names, rewritten for vminitd. Empty stays empty.
pub struct Stdio {
    pub stdin: String,
    pub stdout: String,
    pub stderr: String,
}

/// Opens streams for each named stdio pipe and starts copying; returns the `stream://` URIs for vminitd.
pub fn forward(stream_sock: &Path, prefix: &str, stdin: &str, stdout: &str, stderr: &str) -> Result<Stdio> {
    let mut out = Stdio { stdin: String::new(), stdout: String::new(), stderr: String::new() };
    if !stdin.is_empty() {
        let id = stream_id(prefix, "stdin");
        let sock = start_stream(stream_sock, &id)?;
        let mut pipe = open_pipe(stdin)?;
        std::thread::spawn(move || {
            let mut sock = sock;
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if sock.write_all(&buf[..n]).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = sock.shutdown(std::net::Shutdown::Write);
            // Keep the socket open (not just half-closed) until vminitd closes it
            let mut sink = [0u8; 256];
            while matches!(sock.read(&mut sink), Ok(n) if n > 0) {}
        });
        out.stdin = format!("stream://{id}");
    }
    let copy_out = |path: &str, what: &str| -> Result<String> {
        let id = stream_id(prefix, what);
        let mut sock = start_stream(stream_sock, &id)?;
        let mut pipe = open_pipe(path)?;
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 32 * 1024];
            while let Ok(n) = sock.read(&mut buf) {
                if n == 0 || pipe.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        });
        Ok(format!("stream://{id}"))
    };
    if !stdout.is_empty() {
        out.stdout = copy_out(stdout, "stdout")?;
    }
    if !stderr.is_empty() {
        out.stderr = if stderr == stdout { out.stdout.clone() } else { copy_out(stderr, "stderr")? };
    }
    Ok(out)
}
