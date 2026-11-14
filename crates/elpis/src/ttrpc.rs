//! ttrpc: containerd's lightweight RPC. Each frame is a 10-byte header (big-endian payload length,
//! big-endian stream id, type, flags) followed by a protobuf Request or Response.

use crate::pb::{self, Fields, Pb};
use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, WriteHalf};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use tokio::sync::oneshot;

pub const REQUEST: u8 = 1;
pub const RESPONSE: u8 = 2;
/// A message on a stream (server or client streaming)
pub const DATA: u8 = 3;

/// The sender won't send more on this stream
pub const FLAG_REMOTE_CLOSED: u8 = 0x1;
/// The sender will send more (a client-streaming request)
pub const FLAG_REMOTE_OPEN: u8 = 0x2;
/// The frame carries no message (just flags)
pub const FLAG_NO_DATA: u8 = 0x4;

pub type Frame = (u32, u8, Vec<u8>);

fn header(len: usize, stream: u32, kind: u8, flags: u8) -> [u8; 10] {
    let mut h = [0u8; 10];
    h[..4].copy_from_slice(&(len as u32).to_be_bytes());
    h[4..8].copy_from_slice(&stream.to_be_bytes());
    h[8] = kind;
    h[9] = flags;
    h
}

fn split_header(h: &[u8; 10]) -> (usize, u32, u8) {
    let len = u32::from_be_bytes(h[..4].try_into().unwrap()) as usize;
    let stream = u32::from_be_bytes(h[4..8].try_into().unwrap());
    (len, stream, h[8])
}

pub async fn read_frame_async<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<Frame>> {
    Ok(read_frame_flags_async(r).await?.map(|(stream, kind, _, data)| (stream, kind, data)))
}

/// A frame with its flags: (stream, type, flags, payload).
pub async fn read_frame_flags_async<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<(u32, u8, u8, Vec<u8>)>> {
    let mut h = [0u8; 10];
    match r.read_exact(&mut h).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let (len, stream, kind) = split_header(&h);
    let mut data = vec![0u8; len];
    r.read_exact(&mut data).await?;
    Ok(Some((stream, kind, h[9], data)))
}

pub async fn write_frame_async<W: AsyncWrite + Unpin>(w: &mut W, stream: u32, kind: u8, data: &[u8]) -> io::Result<()> {
    write_frame_flags_async(w, stream, kind, 0, data).await
}

pub async fn write_frame_flags_async<W: AsyncWrite + Unpin>(w: &mut W, stream: u32, kind: u8, flags: u8, data: &[u8]) -> io::Result<()> {
    w.write_all(&header(data.len(), stream, kind, flags)).await?;
    w.write_all(data).await?;
    w.flush().await
}

pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Option<Frame>> {
    let mut h = [0u8; 10];
    match r.read_exact(&mut h) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let (len, stream, kind) = split_header(&h);
    let mut data = vec![0u8; len];
    r.read_exact(&mut data)?;
    Ok(Some((stream, kind, data)))
}

pub fn write_frame<W: Write>(w: &mut W, stream: u32, kind: u8, data: &[u8]) -> io::Result<()> {
    w.write_all(&header(data.len(), stream, kind, 0))?;
    w.write_all(data)?;
    w.flush()
}

/// A ttrpc Response carrying an error status (google.rpc.Status: 1 = code, 2 = message).
pub fn error_response(code: u64, message: &str) -> Vec<u8> {
    Pb::new().msg(1, Pb::new().u64(1, code).str(2, message)).into_vec()
}

#[derive(Debug)]
pub struct RpcError {
    pub code: u64,
    pub message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rpc error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

/// Opens a named pipe, retrying while it doesn't exist yet or all its instances are busy.
pub async fn open_pipe(address: &str, patience: Duration) -> Result<NamedPipeClient> {
    const ERROR_PIPE_BUSY: i32 = 231;
    let started = Instant::now();
    loop {
        match ClientOptions::new().open(address) {
            Ok(pipe) => return Ok(pipe),
            Err(e)
                if (e.raw_os_error() == Some(ERROR_PIPE_BUSY) || e.kind() == io::ErrorKind::NotFound)
                    && started.elapsed() < patience =>
            {
                tokio::time::sleep(Duration::from_millis(50)).await
            }
            Err(e) => return Err(e).with_context(|| format!("opening {address}")),
        }
    }
}

type Pending = Arc<Mutex<HashMap<u32, oneshot::Sender<Vec<u8>>>>>;
type Streams = Arc<Mutex<HashMap<u32, tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>>;

/// Any two-way byte stream a ttrpc client can run over (a named pipe, a bridged AF_UNIX socket, …).
pub trait Transport: AsyncRead + AsyncWrite + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + 'static> Transport for T {}

/// ttrpc client: the shim's Task service, or (in elpis-shim) vminitd's services over vsock.
pub struct Client {
    writer: tokio::sync::Mutex<WriteHalf<Box<dyn Transport + Unpin>>>,
    pending: Pending,
    streams: Streams,
    next: AtomicU32,
    namespace: String,
}

impl Client {
    pub async fn connect(address: &str, namespace: &str) -> Result<Arc<Self>> {
        let pipe = open_pipe(address, Duration::from_secs(10)).await?;
        Ok(Self::over(pipe, namespace))
    }

    /// A client over an already-open stream.
    pub fn over(stream: impl Transport + Unpin, namespace: &str) -> Arc<Self> {
        let stream: Box<dyn Transport + Unpin> = Box::new(stream);
        let (mut reader, writer) = tokio::io::split(stream);
        let pending: Pending = Arc::default();
        let streams: Streams = Arc::default();
        let (responses, messages) = (pending.clone(), streams.clone());
        tokio::spawn(async move {
            while let Ok(Some((stream, kind, flags, data))) = read_frame_flags_async(&mut reader).await {
                match kind {
                    RESPONSE => {
                        if let Some(reply) = responses.lock().unwrap().remove(&stream) {
                            let _ = reply.send(data);
                        }
                        // A streaming call ends with its Response
                        messages.lock().unwrap().remove(&stream);
                    }
                    DATA => {
                        let mut open = messages.lock().unwrap();
                        if flags & FLAG_NO_DATA == 0
                            && let Some(tx) = open.get(&stream)
                        {
                            let _ = tx.send(data);
                        }
                        if flags & FLAG_REMOTE_CLOSED != 0 {
                            open.remove(&stream);
                        }
                    }
                    _ => {}
                }
            }
            // Dropping the senders fails every call still waiting and ends every stream
            responses.lock().unwrap().clear();
            messages.lock().unwrap().clear();
        });
        Arc::new(Self {
            writer: tokio::sync::Mutex::new(writer),
            pending,
            streams,
            next: AtomicU32::new(1),
            namespace: namespace.to_owned(),
        })
    }

    /// A server-streaming call (`rpc M(Req) returns (stream Msg)`): the receiver yields each message's bytes
    /// and ends when the server finishes the stream or the connection closes.
    pub async fn stream(&self, service: &str, method: &str, payload: Vec<u8>) -> Result<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>> {
        let request = Pb::new()
            .str(1, service)
            .str(2, method)
            .bytes(3, &payload)
            .msg(5, Pb::new().str(1, "containerd-namespace-ttrpc").str(2, &self.namespace))
            .into_vec();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut writer = self.writer.lock().await;
        let stream = self.next_stream();
        self.streams.lock().unwrap().insert(stream, tx);
        // We send nothing after the request, as for any call that isn't client-streaming
        write_frame_flags_async(&mut *writer, stream, REQUEST, FLAG_REMOTE_CLOSED, &request).await?;
        Ok(rx)
    }

    /// A bidirectional streaming call (`rpc M(stream Req) returns (stream Msg)`): returns the stream id, for
    /// [`Client::send_data`], and a receiver of the server's messages.
    pub async fn duplex(&self, service: &str, method: &str) -> Result<(u32, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>)> {
        let request = Pb::new()
            .str(1, service)
            .str(2, method)
            .msg(5, Pb::new().str(1, "containerd-namespace-ttrpc").str(2, &self.namespace))
            .into_vec();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut writer = self.writer.lock().await;
        let stream = self.next_stream();
        self.streams.lock().unwrap().insert(stream, tx);
        write_frame_flags_async(&mut *writer, stream, REQUEST, FLAG_REMOTE_OPEN, &request).await?;
        Ok((stream, rx))
    }

    /// Sends one message on a client-streaming call opened with [`Client::duplex`].
    pub async fn send_data(&self, stream: u32, message: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock().await;
        write_frame_flags_async(&mut *writer, stream, DATA, 0, message).await?;
        Ok(())
    }

    /// The next stream id. Call only while holding the writer lock: ttrpc servers reject an id lower than one
    /// they've already seen, so ids must reach the wire in the order they're taken.
    fn next_stream(&self) -> u32 {
        self.next.fetch_add(2, Ordering::SeqCst) // client streams are odd
    }

    pub async fn call(&self, service: &str, method: &str, payload: Vec<u8>, timeout: Duration) -> Result<Vec<u8>> {
        let request = Pb::new()
            .str(1, service)
            .str(2, method)
            .bytes(3, &payload)
            .u64(4, timeout.as_nanos() as u64)
            .msg(5, Pb::new().str(1, "containerd-namespace-ttrpc").str(2, &self.namespace))
            .into_vec();
        let (reply, response) = oneshot::channel();
        let stream = {
            let mut writer = self.writer.lock().await;
            let stream = self.next_stream();
            self.pending.lock().unwrap().insert(stream, reply);
            write_frame_async(&mut *writer, stream, REQUEST, &request).await?;
            stream
        };
        let response = match tokio::time::timeout(timeout + Duration::from_secs(5), response).await {
            Ok(Ok(data)) => data,
            Ok(Err(_)) => return Err(anyhow!("{method}: shim closed the ttrpc connection")),
            Err(_) => {
                self.pending.lock().unwrap().remove(&stream);
                return Err(anyhow!("{method} got no reply in {timeout:?}"));
            }
        };
        let fields = pb::parse(&response)?;
        if let Some(status) = fields.msg(1) {
            let code = status.u(1);
            if code != 0 {
                return Err(RpcError { code, message: status.str(2) }.into());
            }
        }
        Ok(fields.raw(2).map(<[u8]>::to_vec).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server-streaming call: the request goes out with REMOTE_CLOSED, each DATA frame is a message, a
    /// NO_DATA frame carries none, and the Response ends the stream.
    #[tokio::test]
    async fn server_streaming() {
        let (ours, mut server) = tokio::io::duplex(4096);
        let client = Client::over(ours, "ns");
        let mut rx = client.stream("svc.Events", "Stream", Vec::new()).await.unwrap();

        let (stream, kind, flags, data) = read_frame_flags_async(&mut server).await.unwrap().unwrap();
        assert_eq!((kind, flags), (REQUEST, FLAG_REMOTE_CLOSED));
        let request = pb::parse(&data).unwrap();
        assert_eq!((request.str(1).as_str(), request.str(2).as_str()), ("svc.Events", "Stream"));

        write_frame_flags_async(&mut server, stream, DATA, 0, b"one").await.unwrap();
        write_frame_flags_async(&mut server, stream, DATA, FLAG_NO_DATA, b"").await.unwrap();
        write_frame_flags_async(&mut server, stream, DATA, 0, b"two").await.unwrap();
        write_frame_async(&mut server, stream, RESPONSE, &[]).await.unwrap();

        assert_eq!(rx.recv().await.as_deref(), Some(&b"one"[..]));
        assert_eq!(rx.recv().await.as_deref(), Some(&b"two"[..]));
        assert_eq!(rx.recv().await, None);
    }

    /// A bidirectional call: REMOTE_OPEN on the request, then DATA both ways on the same stream.
    #[tokio::test]
    async fn bidirectional_streaming() {
        let (ours, mut server) = tokio::io::duplex(4096);
        let client = Client::over(ours, "ns");
        let (id, mut rx) = client.duplex("svc.Fwd", "Accept").await.unwrap();
        let (stream, kind, flags, _) = read_frame_flags_async(&mut server).await.unwrap().unwrap();
        assert_eq!((stream, kind, flags), (id, REQUEST, FLAG_REMOTE_OPEN));

        write_frame_flags_async(&mut server, id, DATA, 0, b"request").await.unwrap();
        assert_eq!(rx.recv().await.as_deref(), Some(&b"request"[..]));
        client.send_data(id, b"result").await.unwrap();
        let (stream, kind, _, data) = read_frame_flags_async(&mut server).await.unwrap().unwrap();
        assert_eq!((stream, kind, data.as_slice()), (id, DATA, &b"result"[..]));
    }
}
