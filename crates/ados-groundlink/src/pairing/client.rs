//! Relay-side join.
//!
//! 1. Generate an ephemeral X25519 key pair.
//! 2. Resolve the receiver: an explicit host, else mDNS on the mesh interface,
//!    else limited broadcast on the mesh.
//! 3. Send `{"type":"join","device_id","pubkey_hex"}` to the receiver's pairing
//!    port and wait for the sealed invite on the same socket, taking replies only
//!    from the receiver asked (broadcast has no such address: the code alone
//!    authenticates there).
//! 4. Open it with the secret and the six-digit code the receiver's operator
//!    read out, persist the mesh identity, and return the re-key state.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use serde_json::json;
use tokio::net::UdpSocket;
use tokio::time::Instant;

use super::crypto::{decrypt_invite, generate_keypair, invite_context};
use super::env::PairingEnv;
use super::journal;
use super::relay_rekey::{persist_bundle, RelayRekeyState};
use crate::mesh_events::now_ms;

/// Replies that do not open under the code before the join gives up. Each is
/// an online guess at the code, so this bounds a hostile node's odds.
pub const MAX_UNREADABLE_INVITES: u32 = 5;
/// How long the join waits for an invite after sending.
pub const JOIN_TIMEOUT: Duration = Duration::from_secs(45);
const MDNS_TIMEOUT: Duration = Duration::from_secs(3);

/// A successful join.
#[derive(Debug, Clone)]
pub struct Joined {
    pub mesh_id: String,
    pub receiver_host: String,
    pub rekey: RelayRekeyState,
}

/// A failed join: the wire `error_code` and its message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinError {
    pub code: &'static str,
    pub message: String,
}

fn fail(code: &'static str, message: impl Into<String>) -> JoinError {
    JoinError {
        code,
        message: message.into(),
    }
}

fn timed_out(unreadable: u32) -> JoinError {
    if unreadable > 0 {
        fail(
            "E_JOIN_TIMEOUT",
            format!(
                "no invite opened with this code ({unreadable} did not); check the code \
                 the receiver shows and join again"
            ),
        )
    } else {
        fail("E_JOIN_TIMEOUT", "no invite reply received")
    }
}

/// Join the receiver's mesh with the window `code`.
pub async fn request_join(
    env: &PairingEnv,
    code: &str,
    receiver_host: Option<&str>,
    receiver_port: Option<u16>,
    timeout: Duration,
) -> Result<Joined, JoinError> {
    if invite_context(code).is_err() {
        return Err(fail("E_BAD_CODE", "the join code is six digits"));
    }
    let settings = env.settings();
    let device_id = if settings.device_id.is_empty() {
        "relay".to_string()
    } else {
        settings.device_id.clone()
    };
    let port = receiver_port.unwrap_or(env.udp_port);
    let mesh_ip = crate::mdns::iface_ipv4_bounded(&settings.bat_iface).await;

    let mut target: Option<SocketAddr> = None;
    if let Some(host) = receiver_host {
        match tokio::net::lookup_host((host, port)).await {
            Ok(mut addrs) => target = addrs.find(SocketAddr::is_ipv4),
            Err(_) => tracing::warn!(host, "pairing_host_resolve_failed"),
        }
    }
    if target.is_none() {
        let resolved = crate::mdns::resolve_receiver(
            &settings.receiver_mdns_service,
            &settings.bat_iface,
            MDNS_TIMEOUT,
        )
        .await;
        target = resolved
            .and_then(|(ip, _)| ip.parse::<IpAddr>().ok())
            .map(|ip| SocketAddr::new(ip, port));
    }
    let broadcast = target.is_none();
    let target = match target {
        Some(t) => t,
        None if mesh_ip.is_some() => SocketAddr::from((Ipv4Addr::BROADCAST, port)),
        None => {
            return Err(fail(
                "E_MESH_IFACE_DOWN",
                format!("mesh interface {} has no IP", settings.bat_iface),
            ))
        }
    };

    let keys = generate_keypair();
    let bind_ip = mesh_ip.unwrap_or(Ipv4Addr::UNSPECIFIED);
    let sock = UdpSocket::bind((bind_ip, 0))
        .await
        .map_err(|e| fail("E_BIND_FAILED", e.to_string()))?;
    sock.set_broadcast(true)
        .map_err(|e| fail("E_BIND_FAILED", e.to_string()))?;
    let local_port = sock
        .local_addr()
        .map_err(|e| fail("E_BIND_FAILED", e.to_string()))?
        .port();

    let request = json!({
        "type": "join",
        "device_id": device_id,
        "pubkey_hex": hex::encode(keys.public),
    });
    sock.send_to(request.to_string().as_bytes(), target)
        .await
        .map_err(|e| fail("E_JOIN_FAILED", e.to_string()))?;
    tracing::info!(device_id = %device_id, addr = %target, "pairing_join_sent");

    let expected_ip = (!broadcast).then(|| target.ip());
    let deadline = Instant::now() + timeout;
    let mut unreadable = 0u32;
    let mut buf = vec![0u8; 4096];
    loop {
        let (n, src) = match tokio::time::timeout_at(deadline, sock.recv_from(&mut buf)).await {
            Err(_) => return Err(timed_out(unreadable)),
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "pairing_join_recv_failed");
                continue;
            }
            Ok(Ok(r)) => r,
        };
        if expected_ip.is_some_and(|ip| ip != src.ip()) {
            tracing::warn!(expected = ?expected_ip, got = %src.ip(), "pairing_reply_from_other_host");
            continue;
        }
        let bundle = match decrypt_invite(&buf[..n], &keys.secret, code) {
            Ok(b) => b,
            Err(e) => {
                unreadable += 1;
                tracing::warn!(error = %e, count = unreadable, "pairing_invite_unreadable");
                if unreadable >= MAX_UNREADABLE_INVITES {
                    return Err(fail(
                        "E_INVITE_REJECTED",
                        format!(
                            "{unreadable} invites did not open with this code; check the code \
                             the receiver shows and join again"
                        ),
                    ));
                }
                continue;
            }
        };
        persist_bundle(env, &bundle).map_err(|e| fail("E_JOIN_FAILED", e.to_string()))?;
        journal::append(
            &env.pair_events_path,
            journal::JOIN_COMPLETED,
            now_ms(),
            json!({"mesh_id": bundle.mesh_id, "receiver_host": bundle.receiver_mdns_host}),
        );
        tracing::info!(mesh_id = %bundle.mesh_id, receiver_host = %bundle.receiver_mdns_host, "pairing_join_completed");
        return Ok(Joined {
            rekey: RelayRekeyState {
                code: code.to_string(),
                issued_at_ms: bundle.issued_at_ms,
                port: local_port,
                priv_hex: hex::encode(keys.secret.to_bytes()),
            },
            mesh_id: bundle.mesh_id,
            receiver_host: bundle.receiver_mdns_host,
        });
    }
}
