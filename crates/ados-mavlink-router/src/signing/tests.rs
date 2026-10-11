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
