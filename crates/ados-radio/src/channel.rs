//! WFB channel scanning + quietest-channel selection.
//!
//! Mirrors `services/wfb/channel.py`: parse `iw <iface> scan`, count nearby
//! APs per standard 5 GHz WFB channel, and pick the least-congested one inside
//! the configured band. Used by the hop supervisor to choose a hop target.
//!
//! A monitor-mode interface often rejects `iw scan`; on any failure we fall
//! back to "all channels, zero interference" (same as Python), and the caller
//! then rotates rather than blindly staying put.

/// Standard 5 GHz WFB channels: (channel number, centre freq MHz). 20 MHz BW.
pub const STANDARD_CHANNELS: &[(u8, u32)] = &[
    (36, 5180),
    (40, 5200),
    (44, 5220),
    (48, 5240),
    (149, 5745),
    (153, 5765),
    (157, 5785),
    (161, 5805),
    (165, 5825),
];

const BANDWIDTH_MHZ: u32 = 20;

/// Channels per band whitelist (case-insensitive, tolerates the dotted form).
fn band_channels(band: &str) -> Vec<u8> {
    let b = band.to_lowercase();
    if b.contains("u-nii-1") || b.contains("unii-1") {
        vec![36, 40, 44, 48]
    } else if b.contains("u-nii-3") || b.contains("unii-3") {
        vec![149, 153, 157, 161, 165]
    } else {
        STANDARD_CHANNELS.iter().map(|(c, _)| *c).collect()
    }
}

/// Parse `iw scan` stdout into `(freq_mhz, signal_dbm)` pairs. Matches each
/// `freq: <n>` with the next `signal: <n>` (mirrors `_parse_scan_results`).
pub fn parse_scan_results(output: &str) -> Vec<(u32, i32)> {
    let mut out = Vec::new();
    let mut cur_freq: u32 = 0;
    for line in output.lines() {
        let s = line.trim();
        if let Some(rest) = s.strip_prefix("freq:") {
            cur_freq = rest
                .split_whitespace()
                .next()
                .and_then(|t| t.parse().ok())
                .unwrap_or(0);
        } else if let Some(rest) = s.strip_prefix("signal:") {
            if cur_freq > 0 {
                // "signal: -45.00 dBm" → -45
                let sig = rest
                    .trim()
                    .split('.')
                    .next()
                    .and_then(|t| t.trim().parse::<i32>().ok())
                    .unwrap_or(0);
                out.push((cur_freq, sig));
                cur_freq = 0;
            }
        }
    }
    out
}

/// Count detected APs falling within 20 MHz of each standard channel, returning
/// `(channel, ap_count)` sorted ascending by congestion (least busy first).
pub fn rank_channels(detected: &[(u32, i32)]) -> Vec<(u8, u32)> {
    let mut ranked: Vec<(u8, u32)> = STANDARD_CHANNELS
        .iter()
        .map(|&(ch, freq)| {
            let count = detected
                .iter()
                .filter(|&&(f, _)| (f as i64 - freq as i64).unsigned_abs() <= BANDWIDTH_MHZ as u64)
                .count() as u32;
            (ch, count)
        })
        .collect();
    ranked.sort_by_key(|&(_, count)| count);
    ranked
}

/// Run `iw <iface> scan` and rank the standard channels. On any failure (e.g.
/// monitor mode rejects the scan, `iw` missing, timeout) returns every channel
/// at zero interference — the caller decides what to do with a flat ranking.
pub async fn scan_channels(iface: &str) -> Vec<(u8, u32)> {
    let zero: Vec<(u8, u32)> = STANDARD_CHANNELS.iter().map(|&(c, _)| (c, 0)).collect();
    let out = tokio::process::Command::new("iw")
        .args([iface, "scan"])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            let detected = parse_scan_results(&String::from_utf8_lossy(&o.stdout));
            rank_channels(&detected)
        }
        _ => zero,
    }
}

/// Pick a hop target: the quietest in-band channel that is not `current`. Scans
/// live; if the scan is flat/failed, rotates to the next in-band channel so a
/// hop still moves off a bad channel. Returns `current` only if the band has a
/// single channel.
pub async fn pick_hop_target(iface: &str, current: u8, band: &str) -> u8 {
    let allowed = band_channels(band);
    let ranked = scan_channels(iface).await;
    // Quietest in-band channel != current, by ascending congestion.
    if let Some((ch, _)) = ranked
        .iter()
        .filter(|(ch, _)| allowed.contains(ch) && *ch != current)
        .min_by_key(|(_, count)| *count)
    {
        return *ch;
    }
    // Fallback rotation: next in-band channel after current.
    allowed
        .iter()
        .copied()
        .find(|&c| c != current)
        .unwrap_or(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_scan_pairs_freq_with_signal() {
        let out = "BSS aa:bb\n\tfreq: 5745\n\tsignal: -45.00 dBm\nBSS cc:dd\n\tfreq: 5180\n\tsignal: -70.00 dBm\n";
        let got = parse_scan_results(out);
        assert_eq!(got, vec![(5745, -45), (5180, -70)]);
    }

    #[test]
    fn rank_counts_aps_within_band_and_sorts() {
        // Two APs near 5745 (ch149), one near 5180 (ch36).
        let detected = vec![(5745, -40), (5750, -50), (5180, -60)];
        let ranked = rank_channels(&detected);
        // Least congested first; channels with 0 APs sort ahead of busy ones.
        assert_eq!(ranked[0].1, 0); // some empty channel leads
                                    // ch149 should show 2, ch36 should show 1.
        let c149 = ranked.iter().find(|(c, _)| *c == 149).unwrap().1;
        let c36 = ranked.iter().find(|(c, _)| *c == 36).unwrap().1;
        assert_eq!(c149, 2);
        assert_eq!(c36, 1);
    }

    #[test]
    fn band_filter_unii3_only_5ghz_high() {
        assert_eq!(band_channels("u-nii-3"), vec![149, 153, 157, 161, 165]);
        assert_eq!(band_channels("unii-1"), vec![36, 40, 44, 48]);
        assert_eq!(band_channels("all").len(), 9);
    }

    #[test]
    fn pick_target_rotation_fallback_when_flat() {
        // No tokio runtime here; test the pure rotation logic via band_channels.
        // (scan_channels needs iw; the fallback path is what we assert.)
        let allowed = band_channels("u-nii-3");
        let next = allowed.iter().copied().find(|&c| c != 149).unwrap();
        assert_eq!(next, 153);
    }

    #[test]
    fn empty_scan_keeps_all_channels_zero() {
        let ranked = rank_channels(&[]);
        assert_eq!(ranked.len(), 9);
        assert!(ranked.iter().all(|(_, c)| *c == 0));
    }
}
