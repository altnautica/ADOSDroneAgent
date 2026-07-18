//! End-to-end: the offload-link sidecar the reconciler writes drives the
//! perception-tier decision the status surfaces report. Ties the `ados-protocol`
//! sidecar reader to the `ados-offload` tier picker exactly as `/api/status` and
//! the cloud heartbeat do (write a stamped link → read it staleness-gated → feed
//! `TierInputs::for_drone` → `pick_tier`).

use ados_offload::{pick_tier, PerceptionTier, TierInputs};
use ados_protocol::offload_link::{read_offload_link_from, write_offload_link_to, OffloadLink};

/// The status-surface glue, shared by both call sites: read the link, map it to
/// tier inputs for an NPU-less board, pick the tier.
fn tier_from_link(link: Option<&OffloadLink>) -> Option<PerceptionTier> {
    let (paired, bearer_ok) = link
        .map(|l| (l.paired, l.bearer_acceptable))
        .unwrap_or((false, false));
    // An NPU-less board with no CPU-inference declaration: the tier is driven by
    // the offload link alone.
    pick_tier(&TierInputs::for_drone(false, false, paired, bearer_ok))
}

#[test]
fn a_fresh_paired_link_makes_an_npu_less_board_offload() {
    let dir = std::env::temp_dir().join(format!("ados-link-tier-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("offload-link.json");
    let now = 1_700_000_000_000i64;

    let link = OffloadLink::stamped(
        true,
        true,
        Some("192.168.1.5:8092".into()),
        Some("dev-ws".into()),
        Some("coco-yolov8n".into()),
        now,
    );
    write_offload_link_to(&path, &link).unwrap();

    let read = read_offload_link_from(&path, now + 2_000);
    assert!(read.is_some(), "a fresh link must read back");
    assert_eq!(
        tier_from_link(read.as_ref()),
        Some(PerceptionTier::Offload),
        "an NPU-less board with a fresh paired link offloads"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_stale_link_makes_the_board_report_none() {
    let dir = std::env::temp_dir().join(format!("ados-link-tier-stale-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("offload-link.json");
    let gen = 1_700_000_000_000i64;

    let link = OffloadLink::stamped(true, true, Some("h:8092".into()), None, None, gen);
    write_offload_link_to(&path, &link).unwrap();

    // Well past the staleness window ⇒ the reader folds it to absent ⇒ none.
    let read = read_offload_link_from(&path, gen + 60_000);
    assert!(read.is_none(), "a stale link must read as absent");
    assert_eq!(
        tier_from_link(read.as_ref()),
        None,
        "no fresh link ⇒ an NPU-less board reports none (rule 44)"
    );

    std::fs::remove_dir_all(&dir).ok();
}
