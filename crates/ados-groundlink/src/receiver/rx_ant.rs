//! Per-relay fragment counts from the aggregator's `RX_ANT` stats lines.
//!
//! `wfb_rx -a` prints one `RX_ANT` line per (frequency, MCS, bandwidth,
//! antenna) key every stats interval, just before its `PKT` line:
//!
//! ```text
//! <ts>\tRX_ANT\t<freq>:<mcs>:<bw>\t<antenna_id hex>\t<count>:<rssi min:avg:max>:<snr min:avg:max>
//! ```
//!
//! In aggregator mode the antenna id carries the forwarder's source address:
//! `vendor/wfb-ng/src/rx.cpp:539` sets
//! `antenna_id = (uint64_t)ntohl(sockaddr->sin_addr.s_addr) << 32`, and line
//! 542 ORs in `wlan_idx << 8 | ant`. Packets from the local monitor adapter
//! carry no sockaddr, so their upper 32 bits are zero. The upper half is
//! therefore the relay's IPv4 on the mesh, which `/proc/net/arp` maps to the
//! relay's MAC.

use std::collections::HashMap;
use std::net::Ipv4Addr;

/// Parse one `RX_ANT` line into `(antenna_id, packets)`, or `None` for any
/// other line.
pub fn parse_rx_ant_line(line: &str) -> Option<(u64, u64)> {
    let mut cols = line.trim_end().split('\t');
    let _ts = cols.next()?;
    if cols.next()? != "RX_ANT" {
        return None;
    }
    let _freq_mcs_bw = cols.next()?;
    let ant_id = u64::from_str_radix(cols.next()?, 16).ok()?;
    let packets = cols.next()?.split(':').next()?.parse::<u64>().ok()?;
    Some((ant_id, packets))
}

/// Fold one interval's `RX_ANT` lines into packets per antenna id. One antenna
/// appears once per (frequency, MCS, bandwidth) key, so its counts are summed.
pub fn parse_rx_ant<'a>(lines: impl IntoIterator<Item = &'a str>) -> HashMap<u64, u64> {
    let mut out: HashMap<u64, u64> = HashMap::new();
    for (ant_id, packets) in lines.into_iter().filter_map(parse_rx_ant_line) {
        *out.entry(ant_id).or_insert(0) += packets;
    }
    out
}

/// The forwarder IPv4 an aggregator antenna id carries, or `None` for the
/// local monitor adapter (upper 32 bits zero).
pub fn source_ipv4(ant_id: u64) -> Option<Ipv4Addr> {
    let ip = (ant_id >> 32) as u32;
    (ip != 0).then_some(Ipv4Addr::from(ip))
}

/// Packets per forwarding source for one interval. Every antenna of a relay
/// sees the same forwarded packet, so a source's count is the largest of its
/// antennas, not their sum.
pub fn per_source_packets(per_antenna: &HashMap<u64, u64>) -> HashMap<Ipv4Addr, u64> {
    let mut out: HashMap<Ipv4Addr, u64> = HashMap::new();
    for (&ant_id, &packets) in per_antenna {
        if let Some(ip) = source_ipv4(ant_id) {
            let slot = out.entry(ip).or_insert(0);
            *slot = (*slot).max(packets);
        }
    }
    out
}

/// Parse `/proc/net/arp` into IPv4 → lowercase MAC for complete entries on
/// `iface`.
pub fn parse_proc_net_arp(text: &str, iface: &str) -> HashMap<Ipv4Addr, String> {
    let mut out = HashMap::new();
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        let [ip, _hw_type, flags, mac, _mask, dev] = cols.as_slice() else {
            continue;
        };
        if *dev != iface || *flags == "0x0" || *mac == "00:00:00:00:00:00" {
            continue;
        }
        if let Ok(ip) = ip.parse::<Ipv4Addr>() {
            out.insert(ip, mac.to_ascii_lowercase());
        }
    }
    out
}

/// Read the neighbour table for `iface`. Empty when `/proc/net/arp` is
/// unreadable.
pub fn read_arp(iface: &str) -> HashMap<Ipv4Addr, String> {
    std::fs::read_to_string("/proc/net/arp")
        .map(|text| parse_proc_net_arp(&text, iface))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An antenna id the way rx.cpp builds it for a forwarded packet.
    fn ant(ip: Ipv4Addr, wlan_idx: u64, ant: u64) -> u64 {
        ((u32::from(ip) as u64) << 32) | (wlan_idx << 8) | ant
    }

    #[test]
    fn rx_ant_parse_per_source() {
        let relay_a = Ipv4Addr::new(192, 168, 1, 50);
        let relay_b = Ipv4Addr::new(192, 168, 1, 51);
        let lines = [
            format!(
                "1\tRX_ANT\t5745:1:20\t{:x}\t40:-60:-55:-50:20:22:25",
                ant(relay_a, 0, 0)
            ),
            format!(
                "1\tRX_ANT\t5745:1:20\t{:x}\t38:-62:-57:-52:18:20:23",
                ant(relay_a, 0, 1)
            ),
            format!(
                "1\tRX_ANT\t5745:2:20\t{:x}\t2:-61:-61:-61:19:19:19",
                ant(relay_a, 0, 0)
            ),
            format!(
                "1\tRX_ANT\t5745:1:20\t{:x}\t17:-70:-68:-66:10:12:14",
                ant(relay_b, 0, 0)
            ),
            // The local monitor adapter: no forwarder address.
            "1\tRX_ANT\t5745:1:20\t1\t90:-50:-49:-48:25:26:27".to_string(),
            "1\tPKT\t10:0:0:0:10:8:1:0:0:8:1000".to_string(),
        ];
        let per_antenna = parse_rx_ant(lines.iter().map(String::as_str));
        assert_eq!(per_antenna.get(&ant(relay_a, 0, 0)), Some(&42));
        assert_eq!(per_antenna.get(&ant(relay_a, 0, 1)), Some(&38));
        assert_eq!(per_antenna.get(&1), Some(&90));
        assert_eq!(per_antenna.len(), 4);

        let per_source = per_source_packets(&per_antenna);
        assert_eq!(per_source.get(&relay_a), Some(&42));
        assert_eq!(per_source.get(&relay_b), Some(&17));
        assert_eq!(per_source.len(), 2, "the local adapter is not a relay");
    }

    #[test]
    fn non_rx_ant_lines_are_ignored() {
        assert_eq!(parse_rx_ant_line("1\tPKT\t1:2:3"), None);
        assert_eq!(parse_rx_ant_line("garbage"), None);
        assert_eq!(
            parse_rx_ant_line("1\tRX_ANT\t5745:1:20\tzz\t1:0:0:0:0:0:0"),
            None
        );
    }

    #[test]
    fn arp_table_maps_mesh_ips_to_macs() {
        let text =
            "IP address       HW type     Flags       HW address            Mask     Device\n\
                    192.168.1.50     0x1         0x2         AA:BB:CC:DD:EE:01     *        bat0\n\
                    192.168.1.51     0x1         0x0         00:00:00:00:00:00     *        bat0\n\
                    192.168.1.60     0x1         0x2         aa:bb:cc:dd:ee:02     *        eth0\n";
        let arp = parse_proc_net_arp(text, "bat0");
        assert_eq!(
            arp.get(&Ipv4Addr::new(192, 168, 1, 50)).map(String::as_str),
            Some("aa:bb:cc:dd:ee:01")
        );
        assert_eq!(arp.len(), 1, "incomplete and off-mesh entries are skipped");
    }
}
