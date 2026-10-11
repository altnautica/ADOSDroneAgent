//! Agent-held MAVLink 2 signing (`mavlink.agent_signing`, default off).
//!
//! An FC that enforces signing drops the unsigned frames this router
//! originates (heartbeat, stream intervals, the parameter sweep, swarm
//! setpoints). When the operator lets the agent hold the key, the control
//! plane stores it at [`DEFAULT_KEY_PATH`] (0600) and this module signs every
//! agent-originated frame with link id [`AGENT_LINK_ID`]. Client frames are
//! never touched: a GCS signs its own.
//!
//! The signing timestamp (10 µs units since 2015-01-01 UTC) must never repeat
//! or go backwards for a key, or the FC rejects the frames as replays. It is
//! persisted every [`PERSIST_INTERVAL`] and at shutdown, and a restart starts
//! at `max(persisted + 1_000_000, now)`, ten seconds past anything the last
//! run could have used.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ados_protocol::mavlink::ardupilotmega::MavMessage;
use ados_protocol::mavlink::Message;
use sha2::{Digest, Sha256};

/// Where the control plane stores the agent-held key.
pub const DEFAULT_KEY_PATH: &str = "/etc/ados/mavlink-signing.key";
/// Where the last-used signing timestamp is persisted.
pub const DEFAULT_TS_PATH: &str = "/var/lib/ados/mavlink-signing-ts";
/// The MAVLink signing link id agent-originated frames carry.
pub const AGENT_LINK_ID: u8 = 2;
/// How often the timestamp is persisted and the key file re-read.
pub const PERSIST_INTERVAL: Duration = Duration::from_secs(10);
/// The restart margin over the persisted timestamp: ten seconds in 10 µs units.
pub const RESTART_MARGIN: u64 = 1_000_000;

const KEY_LEN: usize = 32;
const EPOCH_2015_UNIX_SECONDS: u64 = 1_420_070_400;
const IFLAG_SIGNED: u8 = 0x01;
const KEY_FILE: &str = "mavlink-signing.key";
const V2_HEADER_LEN: usize = 10;

/// The signing clock now: 10 µs units since 2015-01-01 UTC.
pub fn now_10us(now: SystemTime) -> u64 {
    let unix = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let since = unix.saturating_sub(Duration::from_secs(EPOCH_2015_UNIX_SECONDS));
    u64::try_from(since.as_micros() / 10).unwrap_or(u64::MAX)
}

/// The first timestamp a starting process may use.
pub fn start_timestamp(persisted: Option<u64>, now: u64) -> u64 {
    persisted.map_or(now, |p| p.saturating_add(RESTART_MARGIN).max(now))
}

fn x25(bytes: &[u8], extra: u8) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in bytes.iter().chain(std::iter::once(&extra)) {
        let mut tmp = b ^ (crc & 0xFF) as u8;
        tmp ^= tmp << 4;
        crc = (crc >> 8) ^ (u16::from(tmp) << 8) ^ (u16::from(tmp) << 3) ^ (u16::from(tmp) >> 4);
    }
    crc
}

/// Sign an unsigned MAVLink 2 frame in place: set the signed flag, recompute
/// the checksum over the changed header, and append `link_id`, the 48-bit
/// `timestamp` and the first six bytes of
/// `sha256(key ‖ header ‖ payload ‖ crc ‖ link_id ‖ timestamp)`.
/// A frame that is not a whole, unsigned MAVLink 2 frame is left unchanged.
pub fn sign_frame(frame: &mut Vec<u8>, key: &[u8; KEY_LEN], link_id: u8, timestamp: u64) -> bool {
    if frame.len() < V2_HEADER_LEN || frame[0] != 0xFD || frame[2] & IFLAG_SIGNED != 0 {
        return false;
    }
    let crc_at = V2_HEADER_LEN + frame[1] as usize;
    if frame.len() != crc_at + 2 {
        return false;
    }
    let msg_id = u32::from_le_bytes([frame[7], frame[8], frame[9], 0]);
    frame[2] |= IFLAG_SIGNED;
    let crc = x25(&frame[1..crc_at], MavMessage::extra_crc(msg_id));
    frame[crc_at..].copy_from_slice(&crc.to_le_bytes());
    frame.push(link_id);
    frame.extend_from_slice(&timestamp.to_le_bytes()[..6]);
    let mut h = Sha256::new();
    h.update(key);
    h.update(&frame[..]);
    let digest = h.finalize();
    frame.extend_from_slice(&digest[..6]);
    true
}

#[derive(Default)]
struct Inner {
    key: Option<[u8; KEY_LEN]>,
    /// The last timestamp used; `None` until the first key load seeds it.
    ts: Option<u64>,
    persisted: Option<u64>,
}

/// The router's signing state.
pub struct AgentSigning {
    enabled: bool,
    key_path: PathBuf,
    ts_path: PathBuf,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for AgentSigning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSigning")
            .field("state", &self.state())
            .finish()
    }
}

impl AgentSigning {
    /// Signing state for `enabled` (`mavlink.agent_signing`), reading the key
    /// and timestamp at the given paths. Touches no file until [`Self::refresh`].
    pub fn new(enabled: bool, key_path: PathBuf, ts_path: PathBuf) -> Self {
        Self {
            enabled,
            key_path,
            ts_path,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Signing state at the on-node paths: the key beside the agent config
    /// (`ADOS_CONFIG`'s directory, as the control plane writes it, else
    /// [`DEFAULT_KEY_PATH`]) and the timestamp at [`DEFAULT_TS_PATH`].
    pub fn from_config(enabled: bool) -> Self {
        let key_path = std::env::var("ADOS_CONFIG")
            .ok()
            .and_then(|c| Path::new(&c).parent().map(|d| d.join(KEY_FILE)))
            .unwrap_or_else(|| DEFAULT_KEY_PATH.into());
        Self::new(enabled, key_path, DEFAULT_TS_PATH.into())
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The `agent_signing` status extra: `off`, `active` or `no_key`.
    pub fn state(&self) -> &'static str {
        if !self.enabled {
            "off"
        } else if self.inner().key.is_some() {
            "active"
        } else {
            "no_key"
        }
    }

    /// Whether agent-originated frames are being signed.
    pub fn active(&self) -> bool {
        self.state() == "active"
    }

    /// Re-read the key file: a new key is picked up, a deleted one stops
    /// signing. The first load seeds the timestamp from the persisted one.
    pub fn refresh(&self, now: SystemTime) {
        if !self.enabled {
            return;
        }
        let loaded = read_key(&self.key_path);
        let mut inner = self.inner();
        if let Some(old) = inner.key.as_mut() {
            old.fill(0);
        }
        inner.key = loaded;
        if inner.key.is_some() && inner.ts.is_none() {
            let persisted = read_ts(&self.ts_path);
            inner.ts = Some(start_timestamp(persisted, now_10us(now)));
            inner.persisted = persisted;
        }
    }

    /// Sign `frame` when a key is held; otherwise return it unchanged.
    pub fn sign(&self, frame: Vec<u8>) -> Vec<u8> {
        self.sign_at(frame, SystemTime::now())
    }

    /// [`Self::sign`] at a given wall clock.
    pub fn sign_at(&self, mut frame: Vec<u8>, now: SystemTime) -> Vec<u8> {
        if !self.enabled {
            return frame;
        }
        let mut inner = self.inner();
        let Some(key) = inner.key else {
            return frame;
        };
        let ts = inner.ts.map_or(0, |t| t + 1).max(now_10us(now));
        if sign_frame(&mut frame, &key, AGENT_LINK_ID, ts) {
            inner.ts = Some(ts);
        }
        frame
    }

    /// Persist the last-used timestamp when it moved since the last write.
    pub fn persist(&self) {
        let (ts, persisted) = {
            let inner = self.inner();
            (inner.ts, inner.persisted)
        };
        let Some(ts) = ts.filter(|t| Some(*t) != persisted) else {
            return;
        };
        if let Some(dir) = self.ts_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match ados_protocol::sidecar::write_atomic(&self.ts_path, ts.to_string().as_bytes(), 0o600)
        {
            Ok(()) => self.inner().persisted = Some(ts),
            Err(e) => tracing::warn!(error = %e, "agent_signing_ts_persist_failed"),
        }
    }
}

fn read_key(path: &Path) -> Option<[u8; KEY_LEN]> {
    let mut bytes = std::fs::read(path).ok()?;
    let key = <[u8; KEY_LEN]>::try_from(bytes.as_slice()).ok();
    bytes.fill(0);
    key
}

fn read_ts(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::mavlink::ardupilotmega::HEARTBEAT_DATA;
    use ados_protocol::mavlink::{self, MavHeader};

    fn heartbeat() -> Vec<u8> {
        let h = MavHeader {
            system_id: 1,
            component_id: 191,
            sequence: 4,
        };
        mavlink::serialize_v2(h, &MavMessage::HEARTBEAT(HEARTBEAT_DATA::default())).unwrap()
    }

    fn signer(dir: &Path, key: Option<[u8; KEY_LEN]>) -> AgentSigning {
        let key_path = dir.join("mavlink-signing.key");
        if let Some(k) = key {
            std::fs::write(&key_path, k).unwrap();
        }
        AgentSigning::new(true, key_path, dir.join("state").join("mavlink-signing-ts"))
    }

    #[test]
    fn agent_signs_with_link_id_2() {
        let dir = tempfile::tempdir().unwrap();
        let key = [7u8; KEY_LEN];
        let s = signer(dir.path(), Some(key));
        assert_eq!(s.state(), "no_key");
        let now = SystemTime::now();
        s.refresh(now);
        assert_eq!(s.state(), "active");
        let plain = heartbeat();
        let signed = s.sign_at(plain.clone(), now);
        assert_eq!(signed.len(), plain.len() + 13);
        assert_eq!(signed[2] & IFLAG_SIGNED, IFLAG_SIGNED);
        assert!(mavlink::frame_checksum_ok(&signed));
        let tail = signed.len() - 13;
        assert_eq!(signed[tail], AGENT_LINK_ID);
        let mut h = Sha256::new();
        h.update(key);
        h.update(&signed[..signed.len() - 6]);
        assert_eq!(&signed[signed.len() - 6..], &h.finalize()[..6]);
        // Only the signed flag changed in the header.
        let mut header = plain[..10].to_vec();
        header[2] |= IFLAG_SIGNED;
        assert_eq!(&signed[..10], header.as_slice());
    }

    #[test]
    fn off_or_keyless_leaves_frames_unsigned() {
        let dir = tempfile::tempdir().unwrap();
        let off = AgentSigning::new(false, dir.path().join("k"), dir.path().join("t"));
        assert_eq!(off.state(), "off");
        assert_eq!(off.sign(heartbeat()), heartbeat());
        let keyless = signer(dir.path(), None);
        keyless.refresh(SystemTime::now());
        assert_eq!(keyless.state(), "no_key");
        assert_eq!(keyless.sign(heartbeat()), heartbeat());
    }

    #[test]
    fn deleting_the_key_stops_signing() {
        let dir = tempfile::tempdir().unwrap();
        let s = signer(dir.path(), Some([1u8; KEY_LEN]));
        s.refresh(SystemTime::now());
        assert!(s.active());
        std::fs::remove_file(dir.path().join("mavlink-signing.key")).unwrap();
        s.refresh(SystemTime::now());
        assert_eq!(s.state(), "no_key");
    }

    fn ts_of(frame: &[u8]) -> u64 {
        let t = frame.len() - 12;
        let mut b = [0u8; 8];
        b[..6].copy_from_slice(&frame[t..t + 6]);
        u64::from_le_bytes(b)
    }

    #[test]
    fn timestamp_monotonic_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let first = signer(dir.path(), Some([3u8; KEY_LEN]));
        first.refresh(now);
        let a = ts_of(&first.sign_at(heartbeat(), now));
        let b = ts_of(&first.sign_at(heartbeat(), now));
        assert!(b > a);
        first.persist();
        // A restart whose wall clock has not moved (or went backwards).
        let second = signer(dir.path(), None);
        second.refresh(now - Duration::from_secs(60));
        assert!(second.active());
        let c = ts_of(&second.sign_at(heartbeat(), now - Duration::from_secs(60)));
        assert!(c >= b + RESTART_MARGIN);
        assert_eq!(start_timestamp(None, 5), 5);
        assert_eq!(start_timestamp(Some(1), 5), 1 + RESTART_MARGIN);
    }
}
