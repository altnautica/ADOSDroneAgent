//! batman-adv mesh observation for relay/receiver roles.
//!
//! Polls batman-adv neighbours/gateways and publishes the `mesh-state.json`
//! snapshot. Bringing the mesh up (secure 802.11s/IBSS join, `bat0` bind) and
//! supervising it, including the batman gateway mode, is owned by the
//! `ados-batman` unit (`mesh_manager.py`); this module only observes the result.
//! Subprocess calls go through `batctl::run` (tokio async `Command` +
//! per-call timeout) so a wedged kernel module cannot stall the poll loop.
//!
//! Out of scope here: pairing (that is `pairing`), WFB fragment forwarding
//! (`relay`/`receiver`), and cloud-uplink bringup.

use std::path::Path;
use std::time::Duration;

use crate::paths::{MESH_ID_PATH, MESH_ROLE_PATH};

use super::batctl;
use super::state::MeshSnapshot;

const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Read the on-disk mesh-role sentinel, falling back to `direct` on a missing,
/// unreadable, or unknown value.
///
/// Role transitions (stop/start, the sentinel flip, the `role_changed` event)
/// execute in the supervisor, never in a role unit; this is only the reader.
pub fn get_current_role() -> String {
    get_current_role_at(Path::new(MESH_ROLE_PATH))
}

/// Read the role sentinel from an explicit path (test seam).
pub fn get_current_role_at(path: &Path) -> String {
    if let Ok(text) = std::fs::read_to_string(path) {
        let v = text.trim();
        if matches!(v, "direct" | "relay" | "receiver") {
            return v.to_string();
        }
    }
    "direct".to_string()
}

/// Consecutive isolated polls (mesh up, no neighbour heard) before the snapshot
/// reports a partition. Three polls is six seconds: long enough that one lost
/// OGM interval does not flap the flag, short enough to beat an operator's
/// reaction to a dead relay.
const PARTITION_POLLS: u32 = 3;

/// What one poll pass observed. `None` for a command that failed, so a failure
/// is told apart from a successful empty answer.
#[derive(Debug, Default)]
pub struct MeshPoll {
    pub now_ms: i64,
    /// The batman carrier interface's operstate allows traffic.
    pub bat_up: bool,
    /// `batctl if` output.
    pub hard_ifs: Option<String>,
    /// `batctl n -H` output.
    pub neighbors: Option<String>,
    /// `batctl gwl -H` output.
    pub gateways: Option<String>,
    /// The deployment mesh id from the identity sentinel.
    pub mesh_id: Option<String>,
}

/// The first hard interface `batctl if` lists as active.
fn active_hard_if(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (iface, status) = line.split_once(':')?;
        (status.trim() == "active").then(|| iface.trim().to_string())
    })
}

/// Fold one poll's observations into `snap`.
///
/// A failed `batctl` call clears what it would have refreshed rather than
/// re-serving the previous answer with a fresh `last_poll_ms`: a neighbour list
/// from a module that has since unloaded is not a current reading. `up` needs
/// both the carrier interface up and a neighbour query that answered, and a
/// mesh that is up but has heard no neighbour for [`PARTITION_POLLS`] polls is
/// reported as partitioned.
pub fn apply_poll(snap: &mut MeshSnapshot, poll: MeshPoll) {
    match poll.neighbors.as_deref() {
        Some(out) => snap.neighbors = batctl::parse_neighbors(out, poll.now_ms),
        None => snap.neighbors.clear(),
    }
    match poll.gateways.as_deref() {
        Some(out) => snap.gateways = batctl::parse_gateways(out),
        None => snap.gateways.clear(),
    }
    snap.selected_gateway = snap
        .gateways
        .iter()
        .find(|g| g.selected)
        .map(|g| g.mac.clone());
    snap.mesh_iface = poll
        .hard_ifs
        .as_deref()
        .and_then(active_hard_if)
        .unwrap_or_default();
    snap.mesh_id = poll.mesh_id.unwrap_or_default();
    snap.up = poll.bat_up && poll.neighbors.is_some();
    if snap.up && snap.neighbors.is_empty() {
        snap.isolated_polls = snap.isolated_polls.saturating_add(1);
    } else {
        snap.isolated_polls = 0;
    }
    snap.partition = snap.isolated_polls >= PARTITION_POLLS;
    snap.last_poll_ms = poll.now_ms;
}

/// One poll pass: observe the mesh and fold the result into `snap`.
pub async fn poll_once(snap: &mut MeshSnapshot) {
    let ok = |(rc, out, _e): (i32, String, String)| (rc == 0).then_some(out);
    let timeout = Duration::from_secs(3);
    let operstate = std::fs::read_to_string(format!("/sys/class/net/{}/operstate", snap.bat_iface))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let poll = MeshPoll {
        now_ms: now_ms(),
        // batman-adv reports `unknown` on a live soft interface on some
        // kernels; only an explicit down or a missing interface is down.
        bat_up: matches!(operstate.as_str(), "up" | "unknown"),
        hard_ifs: ok(batctl::run("batctl", &["if"], timeout).await),
        neighbors: ok(batctl::run("batctl", &["n", "-H"], timeout).await),
        gateways: ok(batctl::run("batctl", &["gwl", "-H"], timeout).await),
        mesh_id: std::fs::read_to_string(MESH_ID_PATH)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    };
    apply_poll(snap, poll);
}

/// Wall-clock unix milliseconds.
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The mesh poll loop: refresh + persist every `POLL_INTERVAL` until cancelled.
/// The caller spawns this after a successful `setup` on a relay/receiver node.
///
/// After each persist the same snapshot body is shipped to the logging store as
/// a `mesh.state` event (when an emitter is supplied) so a store-first read
/// never lags the on-disk sidecar. Best-effort: an absent logging daemon drops
/// the event without disturbing the poll loop.
pub async fn run_poll_loop(
    mut snap: MeshSnapshot,
    ingest: Option<ados_protocol::logd::emitter::IngestEmitter>,
) {
    loop {
        poll_once(&mut snap).await;
        if let Err(e) = snap.write() {
            tracing::debug!(error = %e, "mesh_state_write_failed");
        }
        snap.emit(ingest.as_ref());
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_reader_falls_back_to_direct() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("role");
        // Missing → direct.
        assert_eq!(get_current_role_at(&p), "direct");
        // Valid values pass through.
        std::fs::write(&p, "relay\n").unwrap();
        assert_eq!(get_current_role_at(&p), "relay");
        std::fs::write(&p, "receiver\n").unwrap();
        assert_eq!(get_current_role_at(&p), "receiver");
        // Unknown → direct.
        std::fs::write(&p, "bogus\n").unwrap();
        assert_eq!(get_current_role_at(&p), "direct");
    }

    #[test]
    fn now_ms_is_positive() {
        assert!(now_ms() > 0);
    }

    fn live_poll(neighbors: &str) -> MeshPoll {
        MeshPoll {
            now_ms: 10_000,
            bat_up: true,
            hard_ifs: Some("wlan1: active\n".into()),
            neighbors: Some(neighbors.into()),
            gateways: Some("=> 11:22:33:44:55:66 10000/2000 (255)\n".into()),
            mesh_id: Some("ados-abc".into()),
        }
    }

    #[test]
    fn a_live_mesh_reports_up_with_its_identity() {
        let mut snap = MeshSnapshot::new("relay", "bat0", "802.11s");
        apply_poll(&mut snap, live_poll("wlan1 aa:bb:cc:dd:ee:ff 0.5s 240\n"));
        assert!(snap.up);
        assert_eq!(snap.mesh_iface, "wlan1");
        assert_eq!(snap.mesh_id, "ados-abc");
        assert!(!snap.partition);
        assert_eq!(snap.selected_gateway.as_deref(), Some("11:22:33:44:55:66"));
    }

    #[test]
    fn a_failed_batctl_clears_the_lists_instead_of_reserving_them() {
        let mut snap = MeshSnapshot::new("relay", "bat0", "802.11s");
        apply_poll(&mut snap, live_poll("wlan1 aa:bb:cc:dd:ee:ff 0.5s 240\n"));
        apply_poll(
            &mut snap,
            MeshPoll {
                now_ms: 12_000,
                bat_up: true,
                ..MeshPoll::default()
            },
        );
        assert!(!snap.up);
        assert!(snap.neighbors.is_empty());
        assert!(snap.gateways.is_empty());
        assert_eq!(snap.selected_gateway, None);
    }

    #[test]
    fn an_up_mesh_that_hears_nobody_is_reported_partitioned() {
        let mut snap = MeshSnapshot::new("receiver", "bat0", "802.11s");
        for _ in 0..PARTITION_POLLS - 1 {
            apply_poll(&mut snap, live_poll(""));
            assert!(!snap.partition);
        }
        apply_poll(&mut snap, live_poll(""));
        assert!(snap.partition);
        // A neighbour heard again heals it at once.
        apply_poll(&mut snap, live_poll("wlan1 aa:bb:cc:dd:ee:ff 0.5s 240\n"));
        assert!(!snap.partition);
    }
}
