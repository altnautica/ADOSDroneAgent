//! CPU core-cluster probe.
//!
//! Reads `/proc/cpuinfo` `CPU implementer` + `CPU part` fields per logical CPU,
//! folds them into [`Midr`] values, and groups identical MIDRs into
//! big.LITTLE-aware [`CoreCluster`]s (resolving the Cortex name where the part
//! is in the table).

use ados_protocol::hwcaps::{CoreCluster, Evidence, Midr, Probed};

/// Where the per-CPU implementer / part fields live.
const CPUINFO_PATH: &str = "/proc/cpuinfo";

/// Probe the CPU core clusters from `/proc/cpuinfo`.
///
/// Each `processor :` block on an ARM kernel carries a `CPU implementer` and a
/// `CPU part` (both hex). They are folded into a [`Midr`] whose part bits line
/// up with [`Midr::cortex_name`], and runs of consecutive identical MIDRs are
/// grouped into [`CoreCluster`]s so a big.LITTLE rig reports one cluster per
/// core type. The kernel lists cores in MIDR order (the little cores then the
/// big cores, or vice versa), so grouping consecutive runs reconstructs the
/// clusters without sorting.
///
/// Returns [`Probed::Absent`] with [`AbsenceReason::NodeMissing`] when
/// `/proc/cpuinfo` cannot be read or carries no implementer/part fields (a
/// non-ARM kernel exposes neither). On a non-Linux host the file does not exist
/// at all, so the probe is left [`Probed::NotProbed`].
pub fn probe_cores() -> Probed<Vec<CoreCluster>> {
    if !cfg!(target_os = "linux") {
        return Probed::NotProbed;
    }
    match std::fs::read_to_string(CPUINFO_PATH) {
        Ok(text) => parse_cpuinfo(&text),
        Err(_) => Probed::absent(ados_protocol::hwcaps::AbsenceReason::NodeMissing),
    }
}

/// Parse the contents of a `/proc/cpuinfo` into core clusters.
///
/// Split out from [`probe_cores`] so the parser is testable against a fixture
/// string without touching the real filesystem.
fn parse_cpuinfo(text: &str) -> Probed<Vec<CoreCluster>> {
    let midrs = midrs_per_processor(text);
    if midrs.is_empty() {
        // A reachable cpuinfo that exposes no implementer/part (e.g. an x86
        // kernel) is "looked, nothing to identify".
        return Probed::absent(ados_protocol::hwcaps::AbsenceReason::NodeMissing);
    }

    let clusters = cluster_consecutive(&midrs);
    // Evidence records the first cluster's MIDR — enough to audit which silicon
    // answered; the full cluster set is the value.
    let lead_midr = clusters.first().map(|c| c.midr.0).unwrap_or(0);
    Probed::present(clusters, Evidence::ProcCpuinfo { midr: lead_midr })
}

/// Walk the cpuinfo text and return one [`Midr`] per `processor` block that
/// carried both a `CPU implementer` and a `CPU part`.
///
/// The two fields can appear in either order within a block, so they are held
/// pending and combined when the block closes (a blank line or the next
/// `processor :`) or at end of text.
fn midrs_per_processor(text: &str) -> Vec<Midr> {
    let mut out = Vec::new();
    let mut implementer: Option<u32> = None;
    let mut part: Option<u32> = None;

    let flush = |implementer: &mut Option<u32>, part: &mut Option<u32>, out: &mut Vec<Midr>| {
        if let (Some(imp), Some(prt)) = (*implementer, *part) {
            out.push(combine_midr(imp, prt));
        }
        *implementer = None;
        *part = None;
    };

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            // Blank line ends a processor block.
            flush(&mut implementer, &mut part, &mut out);
            continue;
        }
        let Some((key, val)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let val = val.trim();
        match key {
            "processor" => {
                // A new processor block starts; close out any prior one that
                // never saw a blank-line separator.
                flush(&mut implementer, &mut part, &mut out);
            }
            "CPU implementer" => implementer = parse_hex(val),
            "CPU part" => part = parse_hex(val),
            _ => {}
        }
    }
    // End of text closes the final block.
    flush(&mut implementer, &mut part, &mut out);
    out
}

/// Group consecutive identical MIDRs into [`CoreCluster`]s, resolving the
/// Cortex name where the part is in the table (else the raw MIDR hex).
fn cluster_consecutive(midrs: &[Midr]) -> Vec<CoreCluster> {
    let mut clusters: Vec<CoreCluster> = Vec::new();
    for midr in midrs {
        match clusters.last_mut() {
            // Extend the current run; saturate at the u8 ceiling rather than
            // wrap (no real rig has 256 cores, but be defensive).
            Some(last) if last.midr.0 == midr.0 => last.count = last.count.saturating_add(1),
            _ => clusters.push(CoreCluster {
                cortex: midr
                    .cortex_name()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{:#06x}", midr.0)),
                midr: *midr,
                count: 1,
            }),
        }
    }
    clusters
}

/// Combine an implementer + part into a [`Midr`] whose bit layout matches the
/// real `MIDR_EL1` so [`Midr::cortex_name`] (which reads bits `[15:4]`) resolves
/// the part. Implementer sits at `[31:24]`, part at `[15:4]`; variant /
/// architecture / revision are not exposed per-core in cpuinfo and stay zero.
fn combine_midr(implementer: u32, part: u32) -> Midr {
    Midr(((implementer & 0xFF) << 24) | ((part & 0xFFF) << 4))
}

/// Parse a hex field that may or may not carry a `0x` prefix.
fn parse_hex(val: &str) -> Option<u32> {
    let stripped = val.strip_prefix("0x").or_else(|| val.strip_prefix("0X"));
    u32::from_str_radix(stripped.unwrap_or(val), 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An Allwinner A733: 2x Cortex-A76 (part 0xd0b) big cores listed first,
    /// then 6x Cortex-A55 (part 0xd05) little cores. Implementer 0x41 = Arm.
    const A733_CPUINFO: &str = "\
processor\t: 0
BogoMIPS\t: 48.00
Features\t: fp asimd evtstrm aes pmull sha1 sha2 crc32
CPU implementer\t: 0x41
CPU architecture: 8
CPU variant\t: 0x4
CPU part\t: 0xd0b
CPU revision\t: 0

processor\t: 1
CPU implementer\t: 0x41
CPU part\t: 0xd0b
CPU revision\t: 0

processor\t: 2
CPU implementer\t: 0x41
CPU part\t: 0xd05
CPU revision\t: 0

processor\t: 3
CPU implementer\t: 0x41
CPU part\t: 0xd05
CPU revision\t: 0

processor\t: 4
CPU implementer\t: 0x41
CPU part\t: 0xd05
CPU revision\t: 0

processor\t: 5
CPU implementer\t: 0x41
CPU part\t: 0xd05
CPU revision\t: 0

processor\t: 6
CPU implementer\t: 0x41
CPU part\t: 0xd05
CPU revision\t: 0

processor\t: 7
CPU implementer\t: 0x41
CPU part\t: 0xd05
CPU revision\t: 0
";

    #[test]
    fn parses_a733_two_clusters() {
        let probed = parse_cpuinfo(A733_CPUINFO);
        let clusters = probed.value().expect("A733 should probe present");
        assert_eq!(clusters.len(), 2, "A733 is a two-cluster big.LITTLE");

        // big cluster first (kernel order), 2x A76.
        assert_eq!(clusters[0].cortex, "Cortex-A76");
        assert_eq!(clusters[0].count, 2);
        assert_eq!(clusters[0].midr.cortex_name(), Some("Cortex-A76"));

        // little cluster, 6x A55.
        assert_eq!(clusters[1].cortex, "Cortex-A55");
        assert_eq!(clusters[1].count, 6);
        assert_eq!(clusters[1].midr.cortex_name(), Some("Cortex-A55"));
    }

    #[test]
    fn present_evidence_carries_lead_midr() {
        let probed = parse_cpuinfo(A733_CPUINFO);
        match probed {
            Probed::Present { evidence, .. } => {
                // Lead cluster is the A76: implementer 0x41 at [31:24] and part
                // 0xd0b at [15:4] = 0x4100_d0b0.
                assert_eq!(evidence, Evidence::ProcCpuinfo { midr: 0x4100_d0b0 });
            }
            other => panic!("expected Present, got {other:?}"),
        }
    }

    #[test]
    fn fields_in_either_order_within_a_block() {
        // part before implementer must still combine.
        let text = "\
processor\t: 0
CPU part\t: 0xd08
CPU implementer\t: 0x41
";
        let clusters = parse_cpuinfo(text).value().cloned().unwrap();
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].cortex, "Cortex-A72");
        assert_eq!(clusters[0].count, 1);
    }

    #[test]
    fn unknown_part_falls_back_to_raw_midr_hex() {
        let text = "\
processor\t: 0
CPU implementer\t: 0x41
CPU part\t: 0xfff
";
        let clusters = parse_cpuinfo(text).value().cloned().unwrap();
        assert_eq!(clusters.len(), 1);
        // 0x41 << 24 | 0xfff << 4 = 0x4100_fff0.
        assert_eq!(clusters[0].cortex, "0x4100fff0");
        assert_eq!(clusters[0].midr.0, 0x4100_fff0);
    }

    #[test]
    fn identical_non_adjacent_runs_stay_separate_clusters() {
        // A kernel that interleaves core types yields separate runs (we group
        // consecutive, never coalesce a re-appearing MIDR).
        let text = "\
processor\t: 0
CPU implementer\t: 0x41
CPU part\t: 0xd0b

processor\t: 1
CPU implementer\t: 0x41
CPU part\t: 0xd05

processor\t: 2
CPU implementer\t: 0x41
CPU part\t: 0xd0b
";
        let clusters = parse_cpuinfo(text).value().cloned().unwrap();
        assert_eq!(clusters.len(), 3);
        assert_eq!(clusters[0].cortex, "Cortex-A76");
        assert_eq!(clusters[1].cortex, "Cortex-A55");
        assert_eq!(clusters[2].cortex, "Cortex-A76");
    }

    #[test]
    fn cpuinfo_without_arm_fields_is_node_missing() {
        // An x86 cpuinfo carries no implementer/part.
        let text = "\
processor\t: 0
vendor_id\t: GenuineIntel
model name\t: Some CPU
";
        assert!(matches!(
            parse_cpuinfo(text),
            Probed::Absent {
                reason: ados_protocol::hwcaps::AbsenceReason::NodeMissing
            }
        ));
    }

    #[test]
    fn part_without_0x_prefix_still_parses() {
        let text = "\
processor\t: 0
CPU implementer\t: 41
CPU part\t: d03
";
        let clusters = parse_cpuinfo(text).value().cloned().unwrap();
        assert_eq!(clusters[0].cortex, "Cortex-A53");
    }
}
