//! A Windows AF_UNIX listener on its own thread (tokio has no AF_UNIX on Windows), stopped on drop.

use anyhow::{Context, Result};
use socket2::{Domain, SockAddr, Socket, Type};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct Listener {
    path: PathBuf,
    stopped: Arc<AtomicBool>,
}

/// Calls `on_connection` for every accepted connection until the returned [`Listener`] is dropped.
pub fn listen(path: &Path, on_connection: impl Fn(Socket) + Send + 'static) -> Result<Listener> {
    let _ = std::fs::remove_file(path);
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    socket.bind(&SockAddr::unix(path)?).with_context(|| format!("binding {}", path.display()))?;
    socket.listen(16)?;
    let stopped = Arc::new(AtomicBool::new(false));
    let stop = stopped.clone();
    std::thread::spawn(move || {
        while let Ok((conn, _)) = socket.accept() {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            on_connection(conn);
        }
    });
    Ok(Listener { path: path.to_path_buf(), stopped })
}

/// Connects to an AF_UNIX socket.
pub fn connect(path: &Path) -> Result<Socket> {
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    socket.connect(&SockAddr::unix(path)?).with_context(|| format!("connecting to {}", path.display()))?;
    Ok(socket)
}

/// A tokio stream over a blocking AF_UNIX socket: two threads copy between the socket and one end of an
/// in-memory duplex. When the async side shuts down writing, the socket's write half is shut down too.
pub fn into_async(socket: Socket) -> Result<tokio::io::DuplexStream> {
    use std::io::{Read, Write};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (ours, theirs) = tokio::io::duplex(256 * 1024);
    let (mut from_async, mut to_async) = tokio::io::split(theirs);
    let mut reader = socket.try_clone()?;
    let mut writer = socket;
    let handle = tokio::runtime::Handle::current();
    let rt = handle.clone();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || rt.block_on(to_async.write_all(&buf[..n])).is_err() {
                break;
            }
        }
        let _ = rt.block_on(to_async.shutdown());
    });
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n) = handle.block_on(from_async.read(&mut buf)) {
            if n == 0 || writer.write_all(&buf[..n]).is_err() {
                break;
            }
        }
        let _ = writer.shutdown(std::net::Shutdown::Write);
    });
    Ok(ours)
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        // A blocked accept() only returns for a connection, so make one
        if let (Ok(socket), Ok(addr)) = (Socket::new(Domain::UNIX, Type::STREAM, None), SockAddr::unix(&self.path)) {
            let _ = socket.connect(&addr);
        }
        let _ = std::fs::remove_file(&self.path);
    }
}
