//! Receiver byte sources: serial port, UDP port, sensor-hub passthrough.
//!
//! Serial and UDP sources publish what they read as `gnss` frames on the
//! sensor bus (the hub already publishes its own bytes). Every source feeds
//! the shared state through one [`Splitter`] per receiver.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use ados_protocol::shutdown::Shutdown;
use ados_sensorbus::time::mono_ns;
use ados_sensorbus::{v1, Kind, Origin, Publisher, Subscriber};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::config::Source;
use crate::service::Shared;
use crate::stream::Splitter;

const RETRY: Duration = Duration::from_secs(5);
const WRITE_QUEUE: usize = 32;
/// The stream id the hub uses for its GNSS passthrough.
const HUB_GNSS_SRC: &str = "gnss0";

/// Bytes from one receiver into the bus and the shared state.
pub struct Ingest {
    pub shared: Shared,
    pub idx: usize,
    pub id: String,
    pub publisher: Publisher,
    /// Publish the raw bytes as `gnss` frames (not for hub bytes).
    pub publish_raw: bool,
    splitter: Splitter,
}

impl Ingest {
    pub fn new(shared: Shared, idx: usize, publisher: Publisher, publish_raw: bool) -> Self {
        let id = shared.lock().receiver_id(idx).to_string();
        Self {
            shared,
            idx,
            id,
            publisher,
            publish_raw,
            splitter: Splitter::new(),
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        let now = mono_ns();
        if self.publish_raw {
            let raw = v1::GnssRaw {
                t_ns: now,
                bytes: bytes.to_vec(),
            };
            self.publisher.proto(Kind::Gnss, &self.id, &raw);
        }
        let frames = self.splitter.push(bytes);
        if frames.is_empty() {
            return;
        }
        let rtcm: Vec<Vec<u8>> = {
            let mut svc = self.shared.lock();
            frames
                .into_iter()
                .filter_map(|f| svc.on_frame(self.idx, f, now))
                .collect()
        };
        for msg in rtcm {
            self.publisher.raw(Kind::Rtcm, &self.id, msg);
        }
    }
}

/// Run receiver `idx` until shutdown.
pub async fn run(ingest: Ingest, source: Source, bus: PathBuf, shutdown: Shutdown) {
    tokio::select! {
        _ = shutdown.wait() => {}
        _ = drive(ingest, source, bus) => {}
    }
}

async fn drive(mut ingest: Ingest, source: Source, bus: PathBuf) {
    match source {
        Source::Serial { path, baud } => serial(&mut ingest, &path, baud).await,
        Source::Udp { port, peer } => udp(&mut ingest, port, peer).await,
        Source::Hub => hub(&mut ingest, &bus).await,
    }
}

async fn serial(ingest: &mut Ingest, path: &str, baud: u32) {
    use tokio_serial::SerialPortBuilderExt;
    loop {
        let port = match tokio_serial::new(path, baud).open_native_async() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(id = %ingest.id, path, error = %e, "gnss_serial_open_failed");
                tokio::time::sleep(RETRY).await;
                continue;
            }
        };
        tracing::info!(id = %ingest.id, path, baud, "gnss_serial_open");
        let (mut rd, mut wr) = tokio::io::split(port);
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(WRITE_QUEUE);
        ingest.shared.lock().on_attach(ingest.idx, tx, mono_ns());
        let writer = tokio::spawn(async move {
            while let Some(bytes) = rx.recv().await {
                if wr.write_all(&bytes).await.is_err() {
                    break;
                }
            }
        });
        let mut buf = vec![0u8; 4096];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => ingest.push(&buf[..n]),
            }
        }
        ingest.shared.lock().on_detach(ingest.idx);
        writer.abort();
        tracing::warn!(id = %ingest.id, path, "gnss_serial_closed");
        tokio::time::sleep(RETRY).await;
    }
}

/// Bind the UDP source port.
pub async fn bind_udp(port: u16) -> std::io::Result<UdpSocket> {
    UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], port))).await
}

async fn udp(ingest: &mut Ingest, port: u16, peer: Option<SocketAddr>) {
    let sock = loop {
        match bind_udp(port).await {
            Ok(s) => break s,
            Err(e) => {
                tracing::warn!(id = %ingest.id, port, error = %e, "gnss_udp_bind_failed");
                tokio::time::sleep(RETRY).await;
            }
        }
    };
    serve_udp(ingest, sock, peer).await;
}

/// Read datagrams from `sock`. Writes go to the configured `peer`, else to
/// the sender of the latest datagram. The receiver is attached (and sent its
/// configuration) once a destination is known, and again whenever the
/// sender changes, so nothing is written into the void before the first
/// datagram.
pub async fn serve_udp(ingest: &mut Ingest, sock: UdpSocket, peer: Option<SocketAddr>) {
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(WRITE_QUEUE);
    let fixed_peer = peer.is_some();
    let mut dest = peer;
    if fixed_peer {
        ingest
            .shared
            .lock()
            .on_attach(ingest.idx, tx.clone(), mono_ns());
    }
    let mut buf = vec![0u8; 65_536];
    loop {
        tokio::select! {
            r = sock.recv_from(&mut buf) => match r {
                Ok((n, from)) => {
                    if !fixed_peer && dest != Some(from) {
                        tracing::info!(id = %ingest.id, peer = %from, "gnss_udp_peer");
                        dest = Some(from);
                        ingest.shared.lock().on_attach(ingest.idx, tx.clone(), mono_ns());
                    }
                    ingest.push(&buf[..n]);
                }
                Err(e) => tracing::debug!(error = %e, "gnss_udp_recv_error"),
            },
            Some(bytes) = rx.recv() => {
                if let Some(to) = dest {
                    let _ = sock.send_to(&bytes, to).await;
                }
            }
        }
    }
}

async fn hub(ingest: &mut Ingest, bus: &std::path::Path) {
    loop {
        let mut sub = match Subscriber::connect(bus, &[Kind::Gnss]).await {
            Ok(s) => s,
            Err(_) => {
                tokio::time::sleep(RETRY).await;
                continue;
            }
        };
        loop {
            let frame = match sub.recv().await {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => continue,
                Err(_) => break,
            };
            if frame.origin != Origin::Hub || (frame.src != ingest.id && frame.src != HUB_GNSS_SRC)
            {
                continue;
            }
            if let Ok(raw) = frame.decode_proto::<v1::GnssRaw>() {
                ingest.push(&raw.bytes);
            }
        }
        tokio::time::sleep(RETRY).await;
    }
}
