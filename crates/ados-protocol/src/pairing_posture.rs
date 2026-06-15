//! Pairing posture: the data-plane auth primitives shared by the agent's
//! native surfaces.
//!
//! The agent is paired-or-unpaired. Physical presence on the LAN is the gate
//! for an UNPAIRED agent (the claim window); once paired, a data-plane caller
//! reaching the agent from off-box must present the stored pairing key. The
//! local operator (a loopback peer that was not relayed by a proxy or tunnel)
//! already holds shell-level privilege that exceeds API auth and is trusted past
//! the gate.
//!
//! These primitives are protocol-level on purpose: more than one native surface
//! enforces the same posture (the HTTP control surface and the direct MAVLink
//! WebSocket proxy), and a single implementation keeps the two from drifting.
//! Surface-specific concerns (request-rate limiting, the HTTP public-path
//! exempt set, the short-TTL caching wrapper) live with each surface, not here.
//!
//! The pairing state is the agent's `pairing.json` (`{ "paired": bool,
//! "api_key": "..." }`). Read it with [`load_pairing`]; an absent, unreadable,
//! or not-`paired:true`-with-a-key file reads as [`Pairing::Unpaired`] (open),
//! matching the agent's "no key on file means open" stance.

use std::path::Path;

/// Proxy / tunnel relay headers. Their presence means the request was forwarded
/// by a reverse proxy or tunnel (e.g. a Cloudflare Tunnel terminating on
/// 127.0.0.1) rather than originating on this host, so it must NOT qualify for
/// on-box loopback trust. Mirrors the Python middleware's `_FORWARDED_HEADERS`.
pub const FORWARDED_HEADERS: [&str; 4] = [
    "x-forwarded-for",
    "x-real-ip",
    "forwarded",
    "cf-connecting-ip",
];

/// The resolved pairing posture read from `pairing.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pairing {
    /// No pairing on file: the data plane is open (LAN presence is the gate).
    Unpaired,
    /// Paired with this exact key required from an off-box caller.
    Paired(String),
}

/// A data-plane access decision for a paired-or-unpaired agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Admit the connection (unpaired, or on-box, or a valid key).
    Accept,
    /// Reject: the agent is paired, the caller is off-box, and it presented no
    /// key or the wrong key.
    Unauthorized,
}

/// Decide whether a data-plane connection may be admitted, independent of any
/// transport. This is the single posture rule the native surfaces share:
///
/// - **Unpaired ⇒ Accept.** A fresh agent has no key; LAN presence is the gate.
/// - **Paired + on-box ⇒ Accept.** The local operator already holds shell-level
///   privilege that exceeds API auth.
/// - **Paired + off-box + a valid key ⇒ Accept.** Compared in constant time.
/// - **Paired + off-box + a missing or wrong key ⇒ Unauthorized.**
///
/// `on_box` is the resolved [`is_on_box`] result for this peer (loopback and not
/// relayed). `presented_key` is the key the caller supplied (e.g. an
/// `X-ADOS-Key` header), if any.
pub fn data_plane_access(pairing: &Pairing, on_box: bool, presented_key: Option<&str>) -> Access {
    match pairing {
        Pairing::Unpaired => Access::Accept,
        Pairing::Paired(expected) => {
            if on_box {
                return Access::Accept;
            }
            match presented_key {
                Some(presented) if constant_time_eq(presented.as_bytes(), expected.as_bytes()) => {
                    Access::Accept
                }
                _ => Access::Unauthorized,
            }
        }
    }
}

/// Compare two byte slices in time independent of where they first differ, so
/// the bearer-secret check leaks no timing signal about a partial match. A
/// length mismatch is rejected up front (the length of the stored key is not
/// itself a secret); equal-length slices are then folded together with a running
/// difference accumulator that always visits every byte. The compiler is told
/// via `std::hint::black_box` not to short-circuit the loop once a difference is
/// seen.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

/// True when the request originates on this host's loopback interface and was
/// not relayed by a proxy or tunnel. Mirrors the Python middleware's
/// `_is_on_box`: an on-box caller (the local `ados` CLI, a root-owned job)
/// already holds shell-level privilege that strictly exceeds API auth, so it is
/// trusted past the pairing gate. A proxy or tunnel that terminates on loopback
/// is excluded by the forwarding-header check.
///
/// `peer_is_loopback` is whether the connection peer is `127.0.0.1`/`::1`;
/// `has_forwarding_header` is whether any of [`FORWARDED_HEADERS`] is present on
/// the request.
pub fn is_on_box(peer_is_loopback: bool, has_forwarding_header: bool) -> bool {
    peer_is_loopback && !has_forwarding_header
}

/// Load the pairing posture from a `pairing.json`. An absent file, an
/// unreadable file, or a state that is not `paired:true` with a non-empty
/// `api_key` is treated as unpaired (open), matching the agent: when not paired,
/// access is open.
pub fn load_pairing(path: &Path) -> Pairing {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Pairing::Unpaired;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Pairing::Unpaired;
    };
    let paired = value
        .get("paired")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let key = value.get("api_key").and_then(|v| v.as_str());
    match (paired, key) {
        (true, Some(k)) if !k.is_empty() => Pairing::Paired(k.to_string()),
        _ => Pairing::Unpaired,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn write_pairing(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("pairing.json");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    #[test]
    fn constant_time_eq_matches_byte_equality() {
        assert!(constant_time_eq(b"ados_secret", b"ados_secret"));
        assert!(!constant_time_eq(b"ados_secret", b"ados_secre1"));
        assert!(!constant_time_eq(b"ados_secret", b"xdos_secret"));
        assert!(!constant_time_eq(b"ados_secret", b"ados_secret_longer"));
        assert!(!constant_time_eq(b"ados_secret", b"short"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn on_box_trust_is_loopback_and_no_forwarding_header() {
        assert!(is_on_box(true, false));
        assert!(!is_on_box(true, true));
        assert!(!is_on_box(false, false));
        assert!(!is_on_box(false, true));
    }

    #[test]
    fn absent_file_reads_as_unpaired() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_pairing(&dir.path().join("absent.json")),
            Pairing::Unpaired
        );
    }

    #[test]
    fn paired_with_a_key_reads_as_paired() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pairing(dir.path(), r#"{"paired": true, "api_key": "ados_secret"}"#);
        assert_eq!(load_pairing(&path), Pairing::Paired("ados_secret".into()));
    }

    #[test]
    fn paired_without_a_key_reads_as_unpaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pairing(dir.path(), r#"{"paired": true, "api_key": ""}"#);
        assert_eq!(load_pairing(&path), Pairing::Unpaired);
    }

    #[test]
    fn malformed_file_reads_as_unpaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pairing(dir.path(), "this is not json");
        assert_eq!(load_pairing(&path), Pairing::Unpaired);
    }

    #[test]
    fn unpaired_accepts_any_caller() {
        assert_eq!(
            data_plane_access(&Pairing::Unpaired, false, None),
            Access::Accept
        );
        assert_eq!(
            data_plane_access(&Pairing::Unpaired, false, Some("anything")),
            Access::Accept
        );
    }

    #[test]
    fn paired_on_box_accepts_without_a_key() {
        let p = Pairing::Paired("k".into());
        assert_eq!(data_plane_access(&p, true, None), Access::Accept);
    }

    #[test]
    fn paired_off_box_with_a_valid_key_accepts() {
        let p = Pairing::Paired("ados_secret".into());
        assert_eq!(
            data_plane_access(&p, false, Some("ados_secret")),
            Access::Accept
        );
    }

    #[test]
    fn paired_off_box_with_no_key_is_unauthorized() {
        let p = Pairing::Paired("ados_secret".into());
        assert_eq!(data_plane_access(&p, false, None), Access::Unauthorized);
    }

    #[test]
    fn paired_off_box_with_a_wrong_key_is_unauthorized() {
        let p = Pairing::Paired("ados_secret".into());
        assert_eq!(
            data_plane_access(&p, false, Some("wrong")),
            Access::Unauthorized
        );
    }
}
