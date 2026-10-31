//! Relay side of the mesh re-key on revocation.
//!
//! After a join the relay keeps its X25519 secret, the window code and the UDP
//! port its join socket used, in `relay-rekey.json` (0600) with the
//! `issued_at_ms` of the bundle last applied. A listener on that port opens each
//! datagram with the key and code; a bundle that opens and is newer is persisted
//! and `ados-batman` restarted. Anything else is logged and dropped (the
//! receiver sends each re-sealed bundle twice, so the copy is "not newer").

use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use x25519_dalek::StaticSecret;

use super::crypto::decrypt_invite;
use super::env::PairingEnv;
use super::invite::InviteBundle;
use super::rekey::{atomic_write, restart_batman};

/// The re-key state file in the mesh dir.
pub const REKEY_STATE_FILE: &str = "relay-rekey.json";

/// What the relay needs to open a bundle the receiver re-seals to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayRekeyState {
    pub code: String,
    pub issued_at_ms: i64,
    pub port: u16,
    pub priv_hex: String,
}

impl RelayRekeyState {
    /// The stored X25519 secret, `None` when the hex is not 32 bytes.
    pub fn secret(&self) -> Option<StaticSecret> {
        let raw: [u8; 32] = hex::decode(&self.priv_hex).ok()?.try_into().ok()?;
        Some(StaticSecret::from(raw))
    }
}

/// Persist the state owner-only; it holds a private key.
pub fn save_state(mesh_dir: &Path, state: &RelayRekeyState) -> std::io::Result<()> {
    let body = serde_json::to_vec(state).map_err(std::io::Error::other)?;
    atomic_write(&mesh_dir.join(REKEY_STATE_FILE), &body, 0o600)
}

/// The stored state, `None` when missing, malformed or holding a bad key.
pub fn load_state(mesh_dir: &Path) -> Option<RelayRekeyState> {
    let text = std::fs::read_to_string(mesh_dir.join(REKEY_STATE_FILE)).ok()?;
    let state: RelayRekeyState = serde_json::from_str(&text).ok()?;
    state.secret()?;
    Some(state)
}

/// Write a decrypted invite to disk, each file atomically: the mesh id (0644),
/// the PSK (0600), the receiver hint (0644) and, when present, the WFB rx key
/// (0600).
pub fn persist_bundle(env: &PairingEnv, bundle: &InviteBundle) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(&env.mesh_dir)?;
    std::fs::set_permissions(&env.mesh_dir, std::fs::Permissions::from_mode(0o755))?;
    atomic_write(
        &env.mesh_id_path,
        format!("{}\n", bundle.mesh_id).as_bytes(),
        0o644,
    )?;
    atomic_write(&env.mesh_psk_path, &bundle.mesh_psk, 0o600)?;
    let info = json!({
        "mdns_host": bundle.receiver_mdns_host,
        "mdns_port": bundle.receiver_mdns_port,
        "issued_at_ms": bundle.issued_at_ms,
        "expires_at_ms": bundle.expires_at_ms,
    });
    atomic_write(&env.receiver_info_path, info.to_string().as_bytes(), 0o644)?;
    if !bundle.wfb_rx_key.is_empty() {
        atomic_write(&env.wfb_rx_key_path, &bundle.wfb_rx_key, 0o600)?;
    }
    Ok(())
}

/// Opens and applies re-sealed bundles for one stored join.
pub struct RelayRekeyReceiver {
    env: Arc<PairingEnv>,
    state: RelayRekeyState,
    secret: StaticSecret,
}

impl RelayRekeyReceiver {
    pub fn new(env: Arc<PairingEnv>, state: RelayRekeyState) -> Option<Self> {
        let secret = state.secret()?;
        Some(Self { env, state, secret })
    }

    pub fn state(&self) -> &RelayRekeyState {
        &self.state
    }

    /// Apply `blob` if it opens and is newer. `true` when applied.
    pub async fn handle(&mut self, blob: &[u8], src: &str) -> bool {
        let bundle = match decrypt_invite(blob, &self.secret, &self.state.code) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(src, error = %e, "relay_rekey_unreadable");
                return false;
            }
        };
        if bundle.issued_at_ms <= self.state.issued_at_ms {
            tracing::info!(
                issued_at_ms = bundle.issued_at_ms,
                applied_at_ms = self.state.issued_at_ms,
                "relay_rekey_not_newer"
            );
            return false;
        }
        if let Err(e) = persist_bundle(&self.env, &bundle) {
            tracing::error!(error = %e, "relay_rekey_persist_failed");
            return false;
        }
        self.state.issued_at_ms = bundle.issued_at_ms;
        if let Err(e) = save_state(&self.env.mesh_dir, &self.state) {
            tracing::warn!(error = %e, "relay_rekey_state_write_failed");
        }
        tracing::info!(mesh_id = %bundle.mesh_id, src, "relay_rekey_applied");
        restart_batman(&self.env.systemctl).await;
        true
    }
}

/// Bind the re-key port and apply datagrams until the task is aborted.
pub async fn listen(mut receiver: RelayRekeyReceiver) -> std::io::Result<JoinHandle<()>> {
    let port = receiver.state.port;
    let sock = UdpSocket::bind(("0.0.0.0", port)).await?;
    tracing::info!(port, "relay_rekey_listening");
    Ok(tokio::spawn(async move {
        let mut buf = vec![0u8; 4096];
        loop {
            match sock.recv_from(&mut buf).await {
                Ok((n, src)) => {
                    receiver.handle(&buf[..n], &src.ip().to_string()).await;
                }
                Err(e) => {
                    tracing::debug!(error = %e, "relay_rekey_recv_failed");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    }))
}
