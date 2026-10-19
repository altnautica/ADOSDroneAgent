//! Tokio clients for the bus: a fire-and-forget [`Publisher`] that keeps
//! reconnecting, and a [`Subscriber`] stream.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::frame::{decode_body, encode, BusFrame, Kind, Origin};
use crate::MAX_FRAME_BYTES;

/// Frames queued for the writer task before new ones are dropped.
const QUEUE: usize = 512;
const RECONNECT: Duration = Duration::from_millis(500);

/// Read one length-prefixed frame. A frame failing to decode is an
/// `InvalidData` error; the stream stays aligned for the next one.
pub async fn read_frame<R: AsyncRead + Unpin>(
    r: &mut R,
    buf: &mut Vec<u8>,
) -> io::Result<BusFrame> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME_BYTES {
        // Not InvalidData: the stream cannot be realigned past it.
        return Err(io::Error::other("frame too large"));
    }
    buf.resize(len, 0);
    r.read_exact(buf).await?;
    decode_body(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Publishes frames to the broker from a background task. Publishing never
/// blocks: while the bus is down, or the queue is full, frames are dropped
/// and counted (sensor samples are worthless late).
#[derive(Clone)]
pub struct Publisher {
    origin: Origin,
    tx: mpsc::Sender<Vec<u8>>,
    dropped: Arc<AtomicU64>,
}

impl Publisher {
    /// Spawn the writer task on the current tokio runtime. Every frame this
    /// publisher sends is stamped with `origin`.
    pub fn spawn(path: PathBuf, origin: Origin) -> Self {
        let (tx, rx) = mpsc::channel(QUEUE);
        let dropped = Arc::new(AtomicU64::new(0));
        tokio::spawn(writer(path, rx, dropped.clone()));
        Publisher {
            origin,
            tx,
            dropped,
        }
    }

    pub fn origin(&self) -> Origin {
        self.origin
    }

    /// Queue a frame as built. Returns false when it was dropped.
    pub fn send(&self, frame: &BusFrame) -> bool {
        let ok = encode(frame)
            .ok()
            .is_some_and(|bytes| self.tx.try_send(bytes).is_ok());
        if !ok {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }

    /// Queue a prost payload for stream `src`.
    pub fn proto<M: prost::Message>(&self, kind: Kind, src: &str, msg: &M) -> bool {
        self.send(&BusFrame::proto(kind, src, self.origin, msg))
    }

    /// Queue a msgpack payload for stream `src`.
    pub fn msgpack<T: serde::Serialize>(&self, kind: Kind, src: &str, body: &T) -> bool {
        match BusFrame::msgpack(kind, src, self.origin, body) {
            Ok(f) => self.send(&f),
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// Queue raw payload bytes (the `rtcm` kind) for stream `src`.
    pub fn raw(&self, kind: Kind, src: &str, payload: Vec<u8>) -> bool {
        self.send(&BusFrame::new(kind, src, self.origin, payload))
    }

    /// Frames dropped so far (bus down, queue full or encode failure).
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Connect, write one frame and close: for a request handler that must know
/// whether the frame reached the broker (an error means it did not).
pub async fn publish_once(path: &Path, frame: &BusFrame) -> io::Result<()> {
    let bytes = encode(frame).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut stream = UnixStream::connect(path).await?;
    stream.write_all(&bytes).await?;
    stream.shutdown().await
}

async fn writer(path: PathBuf, mut rx: mpsc::Receiver<Vec<u8>>, dropped: Arc<AtomicU64>) {
    loop {
        let mut stream = match UnixStream::connect(&path).await {
            Ok(s) => s,
            Err(_) => {
                // Drain what queued while the bus was down: stale samples.
                while let Ok(_stale) = rx.try_recv() {
                    dropped.fetch_add(1, Ordering::Relaxed);
                }
                tokio::time::sleep(RECONNECT).await;
                if rx.is_closed() {
                    return;
                }
                continue;
            }
        };
        loop {
            let Some(bytes) = rx.recv().await else {
                return;
            };
            if stream.write_all(&bytes).await.is_err() {
                dropped.fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
    }
}

/// A subscription: receives every frame of the requested kinds published by
/// any other bus client.
pub struct Subscriber {
    stream: UnixStream,
    buf: Vec<u8>,
}

impl Subscriber {
    /// Connect and subscribe to `kinds`.
    pub async fn connect(path: &Path, kinds: &[Kind]) -> io::Result<Self> {
        let mut stream = UnixStream::connect(path).await?;
        let sub = BusFrame::msgpack(Kind::Subscribe, "", Origin::Host, &kinds)
            .and_then(|f| encode(&f))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        stream.write_all(&sub).await?;
        Ok(Subscriber {
            stream,
            buf: Vec::new(),
        })
    }

    /// The next frame. An `InvalidData` error is one undecodable frame and
    /// the caller may keep reading; any other error means the bus closed.
    pub async fn recv(&mut self) -> io::Result<BusFrame> {
        read_frame(&mut self.stream, &mut self.buf).await
    }
}
