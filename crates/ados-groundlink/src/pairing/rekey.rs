//! Mesh re-key on relay revocation.
//!
//! Revoking a relay must take it off the mesh fabric, not only refuse its next
//! join: a revoked relay already holding the deployment PSK could otherwise keep
//! joining. On a new revocation the receiver lists the relay's mesh MAC too,
//! rotates the PSK and the mesh id, re-seals the new invite bundle to every
//! relay still paired (unicast UDP to the address its join came from, under the
//! key and window code it joined with), and restarts `ados-batman` so the
//! receiver moves to the new identity.
//!
//! Each approved relay is recorded in `paired-relays.json` (0600, next to the
//! revocation list).

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::crypto::{encrypt_invite, generate_keypair};
use super::env::{PairingEnv, BATMAN_UNIT};
use super::invite::InviteBundle;
use super::revocations;

/// The relay record file in the mesh dir.
pub const PAIRED_RELAYS_FILE: &str = "paired-relays.json";
/// The mesh interface whose neighbour entries name relay MACs.
pub const MESH_IFACE: &str = "bat0";
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(30);
const REVOCATIONS_CACHE_TTL: Duration = Duration::from_secs(30);

/// One approved relay, as the receiver needs it to re-seal an invite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedRelay {
    pub code: String,
    pub device_id: String,
    pub ip: String,
    #[serde(default)]
    pub mac: Option<String>,
    pub port: u16,
    pub relay_pubkey_hex: String,
}

/// The recorded relays. A missing or malformed file (or entry) is skipped.
pub fn load_paired_relays(mesh_dir: &Path) -> Vec<PairedRelay> {
    let Ok(text) = std::fs::read_to_string(mesh_dir.join(PAIRED_RELAYS_FILE)) else {
        return Vec::new();
    };
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str(&text) else {
        return Vec::new();
    };
    items
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect()
}

/// Persist the relay list atomically, owner-only (it holds window codes).
pub fn save_paired_relays(mesh_dir: &Path, relays: &[PairedRelay]) -> std::io::Result<()> {
    let body = serde_json::to_vec(relays).map_err(std::io::Error::other)?;
    atomic_write(&mesh_dir.join(PAIRED_RELAYS_FILE), &body, 0o600)
}

/// Add or replace (by device id) one approved relay.
pub fn record_paired_relay(mesh_dir: &Path, relay: PairedRelay) -> std::io::Result<()> {
    let mut relays: Vec<PairedRelay> = load_paired_relays(mesh_dir)
        .into_iter()
        .filter(|r| r.device_id != relay.device_id)
        .collect();
    relays.push(relay);
    save_paired_relays(mesh_dir, &relays)
}

/// The lowercase MAC the neighbour table holds for `ip` on `iface`.
pub fn arp_mac(arp_table: &Path, ip: &str, iface: &str) -> Option<String> {
    let text = std::fs::read_to_string(arp_table).ok()?;
    text.lines().skip(1).find_map(|line| {
        let cols: Vec<&str> = line.split_whitespace().collect();
        let [addr, _hw, flags, mac, _mask, dev] = cols.as_slice() else {
            return None;
        };
        (*addr == ip && *dev == iface && *flags != "0x0" && *mac != "00:00:00:00:00:00")
            .then(|| mac.to_lowercase())
    })
}

/// Write a fresh 32-byte PSK (0600) and a fresh `ados-<8 hex>` mesh id (0644).
/// Returns the new mesh id; the key is never logged.
pub fn rotate_identity(psk_path: &Path, mesh_id_path: &Path) -> std::io::Result<String> {
    let mut psk = [0u8; 32];
    getrandom::fill(&mut psk).map_err(std::io::Error::other)?;
    atomic_write(psk_path, &psk, 0o600)?;
    let mut tag = [0u8; 4];
    getrandom::fill(&mut tag).map_err(std::io::Error::other)?;
    let mesh_id = format!("ados-{}", hex::encode(tag));
    atomic_write(mesh_id_path, format!("{mesh_id}\n").as_bytes(), 0o644)?;
    tracing::info!(mesh_id = %mesh_id, "mesh_identity_rotated");
    Ok(mesh_id)
}

/// Seal `bundle` to each relay's recorded key under its recorded code and send
/// it twice over UDP. Returns the device ids it was delivered to; a relay whose
/// record cannot seal or whose send fails is logged and skipped.
pub fn reseal(bundle: &InviteBundle, relays: &[PairedRelay]) -> Vec<String> {
    let mut sent = Vec::new();
    for relay in relays {
        match reseal_one(bundle, relay) {
            Ok(()) => sent.push(relay.device_id.clone()),
            Err(e) => {
                tracing::warn!(device_id = %relay.device_id, error = %e, "mesh_reseal_failed")
            }
        }
    }
    tracing::info!(relays = sent.len(), "mesh_resealed");
    sent
}

fn reseal_one(bundle: &InviteBundle, relay: &PairedRelay) -> Result<(), String> {
    let pubkey = hex::decode(&relay.relay_pubkey_hex).map_err(|e| e.to_string())?;
    let keys = generate_keypair();
    let blob =
        encrypt_invite(bundle, &keys.secret, &pubkey, &relay.code).map_err(|e| e.to_string())?;
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    for _ in 0..2 {
        sock.send_to(&blob, (relay.ip.as_str(), relay.port))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Restart the batman-adv bring-up so it re-reads the rotated identity.
pub async fn restart_batman(systemctl: &str) -> bool {
    let outcome =
        ados_proc::run::run(systemctl, &["restart", BATMAN_UNIT], &[], SYSTEMCTL_TIMEOUT).await;
    let ok = outcome.success();
    if !ok {
        tracing::error!(?outcome, "mesh_batman_restart_failed");
    }
    ok
}

/// The revocation set, cached for 30 s so a join flood does not re-read the
/// file per request. Every mutation through this cache updates it at once.
#[derive(Default)]
pub struct RevocationCache {
    cached: Mutex<Option<(Instant, BTreeSet<String>)>>,
}

impl RevocationCache {
    fn guard(&self) -> std::sync::MutexGuard<'_, Option<(Instant, BTreeSet<String>)>> {
        self.cached.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The current set.
    pub fn load(&self, path: &Path) -> BTreeSet<String> {
        let mut cached = self.guard();
        if let Some((at, set)) = cached.as_ref() {
            if at.elapsed() < REVOCATIONS_CACHE_TTL {
                return set.clone();
            }
        }
        let fresh = revocations::load_from(path);
        *cached = Some((Instant::now(), fresh.clone()));
        fresh
    }

    /// Persist `set` and make it the cached value.
    pub fn save(&self, path: &Path, set: BTreeSet<String>) -> std::io::Result<()> {
        revocations::save_to(path, &set)?;
        *self.guard() = Some((Instant::now(), set));
        Ok(())
    }

    /// Whether `device_id` is revoked.
    pub fn is_revoked(&self, path: &Path, device_id: &str) -> bool {
        self.load(path).contains(device_id)
    }
}

/// Revoke `device_id` and re-key the mesh. `true` when this call revoked it,
/// `false` when it already was (nothing written, no re-key). Each re-key step is
/// logged; a failed step does not undo the revocation.
pub async fn revoke(
    env: &PairingEnv,
    cache: &RevocationCache,
    device_id: &str,
) -> std::io::Result<bool> {
    let path = env.revocations_path();
    let mut set = cache.load(&path);
    if set.contains(device_id) {
        return Ok(false);
    }
    let relays = load_paired_relays(&env.mesh_dir);
    set.insert(device_id.to_string());
    if let Some(target) = relays.iter().find(|r| r.device_id == device_id) {
        let mac = target
            .mac
            .clone()
            .or_else(|| arp_mac(&env.arp_table, &target.ip, MESH_IFACE));
        if let Some(mac) = mac {
            set.insert(mac.to_lowercase());
        }
    }
    cache.save(&path, set.clone())?;
    tracing::info!(device_id, "pairing_revoked");

    let remaining: Vec<PairedRelay> = relays
        .into_iter()
        .filter(|r| !set.contains(&r.device_id))
        .collect();
    rekey_mesh(env, &remaining).await;
    Ok(true)
}

async fn rekey_mesh(env: &PairingEnv, remaining: &[PairedRelay]) {
    let (psk_path, mesh_id_path) = env.identity_paths();
    if let Err(e) = rotate_identity(&psk_path, &mesh_id_path) {
        tracing::error!(error = %e, "mesh_identity_rotation_failed");
        return;
    }
    if let Err(e) = save_paired_relays(&env.mesh_dir, remaining) {
        tracing::warn!(error = %e, "paired_relays_write_failed");
    }
    match env.build_invite_bundle() {
        Some(bundle) => {
            reseal(&bundle, remaining);
        }
        None => tracing::error!("mesh_reseal_skipped_no_bundle"),
    }
    restart_batman(&env.systemctl).await;
}

/// Write `data` to `path` through a tmp file and a rename, with `mode` set
/// before the rename so the file never exists more open.
pub fn atomic_write(path: &Path, data: &[u8], mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, path)
}
