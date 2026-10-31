//! Receiver-side accept-window state machine.
//!
//! Opening the window mints an X25519 key pair and a six-digit code, binds the
//! UDP join listener (on `bat0` when it has an address), and starts an expiry
//! task plus a 5 s countdown tick. Join datagrams land in the pending list; an
//! operator approval seals the invite bundle to the relay's key under the code
//! and sends it back twice on the same socket. Every transition is journalled.
//!
//! ```text
//! idle -> accept_window_open -> request_received -> approved
//!                  |-> closed (deadline or operator close)
//! ```

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use x25519_dalek::StaticSecret;

use super::crypto::{encrypt_invite, generate_keypair};
use super::env::PairingEnv;
use super::invite::InviteBundle;
use super::journal;
use super::rekey::{self, PairedRelay, RevocationCache, MESH_IFACE};
use super::window::{closed_snapshot, parse_join, AcceptWindow, PendingRequest};
use crate::mesh_events::now_ms;

const TICK_PERIOD: Duration = Duration::from_secs(5);
const INVITE_RESEND_GAP: Duration = Duration::from_millis(100);

/// The opened window's display fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedWindow {
    pub opened_at_ms: i64,
    pub closes_at_ms: i64,
    pub code: String,
}

#[derive(Default)]
struct Inner {
    window: Option<AcceptWindow>,
    secret: Option<StaticSecret>,
    socket: Option<Arc<UdpSocket>>,
    listener: Option<JoinHandle<()>>,
    timers: Vec<JoinHandle<()>>,
}

/// The single owner of the receiver's pairing state.
pub struct MeshPairingManager {
    env: Arc<PairingEnv>,
    revoked: Arc<RevocationCache>,
    inner: Mutex<Inner>,
}

impl MeshPairingManager {
    pub fn new(env: Arc<PairingEnv>, revoked: Arc<RevocationCache>) -> Arc<Self> {
        Arc::new(Self {
            env,
            revoked,
            inner: Mutex::new(Inner::default()),
        })
    }

    /// Open (or return the still-open) window. A failed UDP bind closes the
    /// window again and is an error, so the caller never sees an open window no
    /// relay can reach.
    pub async fn open_window(self: &Arc<Self>, duration_s: u64) -> Result<OpenedWindow, String> {
        let mut inner = self.inner.lock().await;
        if let Some(w) = inner.window.as_ref().filter(|w| w.is_open()) {
            return Ok(opened(w));
        }
        let keys = generate_keypair();
        let now = now_ms();
        let window = AcceptWindow::open(now, duration_s);
        let deadline = window.deadline;
        let info = opened(&window);
        inner.window = Some(window);
        inner.secret = Some(keys.secret);
        journal::append(
            &self.env.pair_events_path,
            journal::ACCEPT_WINDOW_OPENED,
            now,
            json!({"duration_s": duration_s}),
        );
        tracing::info!(duration_s, "pairing_window_opened");

        if inner.socket.is_none() {
            match bind_join_socket(self.env.udp_port).await {
                Ok(sock) => {
                    let sock = Arc::new(sock);
                    inner.listener = Some(tokio::spawn(Arc::clone(self).listen(Arc::clone(&sock))));
                    inner.socket = Some(sock);
                }
                Err(e) => {
                    tracing::error!(port = self.env.udp_port, error = %e, "pairing_bind_failed");
                    self.close_locked(&mut inner);
                    return Err(format!(
                        "pairing UDP bind failed on port {}",
                        self.env.udp_port
                    ));
                }
            }
        }
        for t in inner.timers.drain(..) {
            t.abort();
        }
        inner
            .timers
            .push(tokio::spawn(Arc::clone(self).expire_at(deadline)));
        inner
            .timers
            .push(tokio::spawn(Arc::clone(self).tick_while_open(deadline)));
        Ok(info)
    }

    /// Close the window if one exists.
    pub async fn close_window(&self) {
        let mut inner = self.inner.lock().await;
        if inner.window.is_some() {
            self.close_locked(&mut inner);
        }
    }

    pub async fn is_window_open(&self) -> bool {
        self.inner
            .lock()
            .await
            .window
            .as_ref()
            .is_some_and(AcceptWindow::is_open)
    }

    /// The `snapshot` body: `{"open": false}` with no window, else the window.
    pub async fn snapshot(&self) -> Value {
        match &self.inner.lock().await.window {
            Some(w) => w.snapshot(),
            None => closed_snapshot(),
        }
    }

    /// The bound join-listener port, while a window holds the socket.
    pub async fn bound_udp_port(&self) -> Option<u16> {
        let inner = self.inner.lock().await;
        inner
            .socket
            .as_ref()
            .and_then(|s| s.local_addr().ok())
            .map(|a| a.port())
    }

    /// Record a join request. `false` when the device is revoked or no window
    /// is open.
    pub async fn submit_request(
        &self,
        device_id: &str,
        relay_pubkey: [u8; 32],
        remote_addr: SocketAddr,
    ) -> bool {
        if self
            .revoked
            .is_revoked(&self.env.revocations_path(), device_id)
        {
            journal::append(
                &self.env.pair_events_path,
                journal::REVOKED,
                now_ms(),
                json!({"device_id": device_id}),
            );
            return false;
        }
        let mut inner = self.inner.lock().await;
        let Some(window) = inner.window.as_mut().filter(|w| w.is_open()) else {
            return false;
        };
        if window.pending_for(device_id).is_none() {
            let now = now_ms();
            window.pending.push(PendingRequest {
                device_id: device_id.to_string(),
                relay_pubkey,
                remote_addr,
                received_at_ms: now,
            });
            journal::append(
                &self.env.pair_events_path,
                journal::JOIN_REQUEST_RECEIVED,
                now,
                json!({"device_id": device_id}),
            );
            tracing::info!(device_id, "pairing_join_request");
        }
        true
    }

    /// Seal `bundle` to a pending relay and send it back. `Ok(None)` when the
    /// device is not pending or the window is closed.
    pub async fn approve(
        &self,
        device_id: &str,
        bundle: &InviteBundle,
    ) -> Result<Option<Vec<u8>>, String> {
        let mut inner = self.inner.lock().await;
        let Inner {
            window,
            secret,
            socket,
            ..
        } = &mut *inner;
        let (Some(window), Some(secret)) = (window.as_mut(), secret.as_ref()) else {
            return Ok(None);
        };
        if !window.is_open() {
            return Ok(None);
        }
        let Some(request) = window.pending_for(device_id).cloned() else {
            return Ok(None);
        };
        let blob = encrypt_invite(bundle, secret, &request.relay_pubkey, &window.code)
            .map_err(|e| e.to_string())?;
        let now = now_ms();
        window.approvals.insert(device_id.to_string(), now);
        let mut paired = PairedRelay {
            code: window.code.clone(),
            device_id: device_id.to_string(),
            ip: request.remote_addr.ip().to_string(),
            mac: None,
            port: request.remote_addr.port(),
            relay_pubkey_hex: hex::encode(request.relay_pubkey),
        };
        journal::append(
            &self.env.pair_events_path,
            journal::JOIN_APPROVED,
            now,
            json!({"device_id": device_id}),
        );
        tracing::info!(device_id, "pairing_join_approved");
        let socket = socket.clone();
        drop(inner);

        // UDP is lossy: send twice; the relay drops a copy that does not open.
        match socket {
            Some(sock) => {
                for attempt in 1..=2 {
                    if let Err(e) = sock.send_to(&blob, request.remote_addr).await {
                        tracing::warn!(device_id, attempt, error = %e, "pairing_invite_send_failed");
                        break;
                    }
                    tracing::info!(device_id, addr = %request.remote_addr, attempt, "pairing_invite_sent");
                    if attempt == 1 {
                        tokio::time::sleep(INVITE_RESEND_GAP).await;
                    }
                }
            }
            None => tracing::warn!(device_id, "pairing_invite_no_socket"),
        }
        // Remembered so a later revocation can re-seal the rotated bundle.
        paired.mac = rekey::arp_mac(&self.env.arp_table, &paired.ip, MESH_IFACE);
        if let Err(e) = rekey::record_paired_relay(&self.env.mesh_dir, paired) {
            tracing::warn!(device_id, error = %e, "paired_relay_record_failed");
        }
        Ok(Some(blob))
    }

    fn close_locked(&self, inner: &mut Inner) {
        inner.window = None;
        inner.secret = None;
        inner.socket = None;
        if let Some(l) = inner.listener.take() {
            l.abort();
        }
        for t in inner.timers.drain(..) {
            t.abort();
        }
        journal::append(
            &self.env.pair_events_path,
            journal::ACCEPT_WINDOW_CLOSED,
            now_ms(),
            json!({}),
        );
        tracing::info!("pairing_window_closed");
    }

    async fn listen(self: Arc<Self>, sock: Arc<UdpSocket>) {
        let mut buf = vec![0u8; 4096];
        loop {
            match sock.recv_from(&mut buf).await {
                Ok((n, addr)) => {
                    if let Some((device_id, pubkey)) = parse_join(&buf[..n]) {
                        self.submit_request(&device_id, pubkey, addr).await;
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "pairing_udp_error");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    async fn expire_at(self: Arc<Self>, deadline: Instant) {
        tokio::time::sleep_until(deadline.into()).await;
        let mut inner = self.inner.lock().await;
        let current = inner
            .window
            .as_ref()
            .is_some_and(|w| w.deadline == deadline && !w.is_open());
        if current {
            // This task is one of the timers close_locked aborts; detach it first.
            inner.timers.clear();
            self.close_locked(&mut inner);
        }
    }

    async fn tick_while_open(self: Arc<Self>, deadline: Instant) {
        loop {
            tokio::time::sleep(TICK_PERIOD).await;
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return;
            };
            if remaining.is_zero() {
                return;
            }
            {
                let inner = self.inner.lock().await;
                if inner.window.as_ref().map(|w| w.deadline) != Some(deadline) {
                    return;
                }
            }
            journal::append(
                &self.env.pair_events_path,
                journal::ACCEPT_WINDOW_TICK,
                now_ms(),
                json!({"remaining_seconds": remaining.as_secs()}),
            );
        }
    }
}

fn opened(w: &AcceptWindow) -> OpenedWindow {
    OpenedWindow {
        opened_at_ms: w.opened_at_ms,
        closes_at_ms: w.closes_at_ms,
        code: w.code.clone(),
    }
}

/// Bind the join listener: on the mesh interface's address (scoped to the
/// interface) when it has one, so off-mesh hosts on a shared LAN cannot reach
/// it, else on every address with a warning. Address and port reuse let a fast
/// restart reclaim the port.
async fn bind_join_socket(port: u16) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let bat_ip = crate::mdns::iface_ipv4_bounded(MESH_IFACE).await;
    if bat_ip.is_none() {
        tracing::warn!(
            port,
            "pairing_bat0_ip_unavailable_falling_back: binding 0.0.0.0, the mesh carrier has no IPv4 address yet"
        );
    }
    let ip = bat_ip.unwrap_or(Ipv4Addr::UNSPECIFIED);
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    let _ = sock.set_reuse_port(true);
    #[cfg(target_os = "linux")]
    if bat_ip.is_some() {
        if let Err(e) = sock.bind_device(Some(MESH_IFACE.as_bytes())) {
            tracing::debug!(error = %e, "pairing_bindtodevice_failed");
        }
    }
    sock.set_nonblocking(true)?;
    sock.bind(&SocketAddr::from((ip, port)).into())?;
    let sock = UdpSocket::from_std(sock.into())?;
    tracing::info!(addr = %ip, port, "pairing_bind_ok");
    Ok(sock)
}
