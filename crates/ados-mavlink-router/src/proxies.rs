//! Direct GCS transport proxies.
//!
//! Mirror the Python TCP (`tcp_proxy.py`, port 5760) and UDP (`udp_proxy.py`,
//! ports 14550/14551) proxies: a GCS connects directly and exchanges raw
//! MAVLink frames with the FC, bypassing the cloud relay. Each proxy relays the
//! FC frame stream (via [`FcConnection::subscribe`]) out to its clients and
//! forwards client bytes back to the FC with [`FcConnection::send_bytes`].
//!
//! The WebSocket proxy (8765) is a separate follow-up (needs a WS dependency).

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Mutex, Notify};
use tokio_tungstenite::tungstenite::Message;

use crate::connection::FcConnection;

/// TCP MAVLink proxy. Binds `0.0.0.0:<port>` and serves each client a copy of
/// the FC frame stream while forwarding its bytes to the FC.
pub async fn run_tcp_proxy(fc: Arc<FcConnection>, port: u16, cancel: Arc<Notify>) {
    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(port, error = %e, "tcp_proxy_bind_failed");
            return;
        }
    };
    tracing::info!(port, "tcp_proxy_listening");
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, _addr)) = accepted {
                    tokio::spawn(handle_tcp_client(fc.clone(), stream));
                }
            }
            _ = cancel.notified() => return,
        }
    }
}

async fn handle_tcp_client(fc: Arc<FcConnection>, stream: tokio::net::TcpStream) {
    let (mut rd, mut wr) = stream.into_split();
    let mut rx = fc.subscribe();

    // FC -> client.
    let writer = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    if wr.write_all(&frame).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    });

    // client -> FC.
    let mut buf = [0u8; 4096];
    loop {
        match rd.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => fc.send_bytes(&buf[..n]).await,
        }
    }
    writer.abort();
}

/// UDP MAVLink proxy. Binds `0.0.0.0:<port>`, learns each GCS peer from its
/// inbound datagrams, forwards peer bytes to the FC, and sends FC frames to
/// every learned peer.
pub async fn run_udp_proxy(fc: Arc<FcConnection>, port: u16, cancel: Arc<Notify>) {
    let sock = match UdpSocket::bind(("0.0.0.0", port)).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::warn!(port, error = %e, "udp_proxy_bind_failed");
            return;
        }
    };
    tracing::info!(port, "udp_proxy_listening");
    let peers: Arc<Mutex<HashSet<SocketAddr>>> = Arc::new(Mutex::new(HashSet::new()));

    // FC -> peers.
    let send_sock = sock.clone();
    let send_peers = peers.clone();
    let mut rx = fc.subscribe();
    let sender = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    let targets: Vec<SocketAddr> =
                        send_peers.lock().await.iter().copied().collect();
                    for peer in targets {
                        let _ = send_sock.send_to(&frame, peer).await;
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    });

    // peers -> FC.
    let mut buf = [0u8; 4096];
    loop {
        tokio::select! {
            recv = sock.recv_from(&mut buf) => {
                if let Ok((n, addr)) = recv {
                    peers.lock().await.insert(addr);
                    fc.send_bytes(&buf[..n]).await;
                }
            }
            _ = cancel.notified() => {
                sender.abort();
                return;
            }
        }
    }
}

/// WebSocket MAVLink proxy. Binds `0.0.0.0:<port>` and bridges binary WebSocket
/// frames to/from the FC, the way a browser GCS connects. Text/ping frames are
/// ignored; only binary frames carry MAVLink.
pub async fn run_ws_proxy(fc: Arc<FcConnection>, port: u16, cancel: Arc<Notify>) {
    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(port, error = %e, "ws_proxy_bind_failed");
            return;
        }
    };
    tracing::info!(port, "ws_proxy_listening");
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, _addr)) = accepted {
                    tokio::spawn(handle_ws_client(fc.clone(), stream));
                }
            }
            _ = cancel.notified() => return,
        }
    }
}

async fn handle_ws_client(fc: Arc<FcConnection>, stream: tokio::net::TcpStream) {
    let ws = match tokio_tungstenite::accept_async(stream).await {
        Ok(w) => w,
        Err(_) => return,
    };
    let (mut write, mut read) = ws.split();
    let mut rx = fc.subscribe();

    // FC -> client (binary frames).
    let writer = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    if write.send(Message::Binary(frame)).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    });

    // client -> FC (binary frames only; ignore text/ping/pong).
    while let Some(msg) = read.next().await {
        match msg {
            Ok(Message::Binary(data)) => fc.send_bytes(&data).await,
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }
    writer.abort();
}
