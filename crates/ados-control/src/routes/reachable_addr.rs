//! Tell an address that can answer from one that only looks like it can.
//!
//! ## The failure this names
//!
//! A node with two interfaces in the SAME subnet answers ARP on both, so both
//! addresses look alive from another machine. Only one of them works. Replies
//! leave by the default route, sourced from that interface's address, and a
//! client that opened a connection to the other address drops them as coming
//! from somewhere it was not talking to. The node is healthy, the address
//! resolves, and nothing connects.
//!
//! That is worse than an address that does not resolve at all, because a name
//! that resolves reads as a reachable node. It cost a real diagnostic session:
//! a ground station was declared down while it was serving happily on its other
//! address, and the published `.local` name pointed at the dead one.
//!
//! ## Why this is a rule and not a lookup
//!
//! Whether an address can answer is decidable from what the node already knows:
//! its own addresses, and which interface the default route leaves by. No probe
//! is needed, and a probe would be worse — it would answer for the machine
//! doing the probing rather than for every client.

/// One locally-assigned IPv4 address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalAddr {
    pub iface: String,
    pub addr: String,
    /// Network prefix length, as in `/24`.
    pub prefix: u8,
}

/// The interface the default route leaves by: the lowest-metric `0.0.0.0`
/// destination in a `/proc/net/route` dump.
///
/// Lowest metric wins because that is how the kernel picks. A node with two
/// default routes is not misconfigured -- it is a node with a wired and a
/// wireless path -- and the metric is exactly the statement of which one is
/// preferred.
pub fn default_route_iface(proc_net_route: &str) -> Option<String> {
    let mut best: Option<(u32, String)> = None;
    for line in proc_net_route.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 7 || f[1] != "00000000" {
            continue;
        }
        let metric: u32 = f[6].parse().unwrap_or(u32::MAX);
        if best.as_ref().is_none_or(|(m, _)| metric < *m) {
            best = Some((metric, f[0].to_string()));
        }
    }
    best.map(|(_, iface)| iface)
}

/// Addresses that will not answer an off-box client, and why.
///
/// An address is unreachable when it sits on a DIFFERENT interface from the
/// default route while sharing that route's subnet. The sharing is what breaks
/// it: the kernel has one route for that subnet, so replies to either address
/// leave by the same interface with the same source, and only one of the two
/// addresses is ever the source.
///
/// An address on another interface in a DIFFERENT subnet is fine and is not
/// reported -- that is an ordinary second network, reached by its own route.
pub fn unreachable_addrs(addrs: &[LocalAddr], default_iface: &str) -> Vec<String> {
    let Some(primary) = addrs.iter().find(|a| a.iface == default_iface) else {
        // Nothing is known to be preferred, so nothing can be called worse.
        return Vec::new();
    };
    addrs
        .iter()
        .filter(|a| a.iface != default_iface)
        .filter(|a| same_subnet(&a.addr, a.prefix, &primary.addr, primary.prefix))
        .map(|a| a.addr.clone())
        .collect()
}

/// The address an off-box client should be given: the one on the default-route
/// interface, else the first that is not shadowed by the rule above.
pub fn preferred_addr(addrs: &[LocalAddr], default_iface: &str) -> Option<String> {
    if let Some(a) = addrs.iter().find(|a| a.iface == default_iface) {
        return Some(a.addr.clone());
    }
    let bad = unreachable_addrs(addrs, default_iface);
    addrs
        .iter()
        .find(|a| !bad.contains(&a.addr))
        .map(|a| a.addr.clone())
}

/// Whether two addresses fall in the same network, using the narrower prefix.
///
/// The narrower one decides: if either side considers the other to be on its
/// own network, the kernel has a single route covering both, which is the
/// condition that makes the second address unanswerable.
fn same_subnet(a: &str, a_prefix: u8, b: &str, b_prefix: u8) -> bool {
    let (Some(a), Some(b)) = (to_u32(a), to_u32(b)) else {
        return false;
    };
    let prefix = a_prefix.min(b_prefix).min(32);
    if prefix == 0 {
        return true;
    }
    let mask = u32::MAX << (32 - prefix as u32);
    a & mask == b & mask
}

fn to_u32(s: &str) -> Option<u32> {
    let mut out: u32 = 0;
    let mut seen = 0;
    for part in s.split('.') {
        let n: u32 = part.parse().ok()?;
        if n > 255 {
            return None;
        }
        out = (out << 8) | n;
        seen += 1;
    }
    (seen == 4).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(iface: &str, addr: &str, prefix: u8) -> LocalAddr {
        LocalAddr {
            iface: iface.into(),
            addr: addr.into(),
            prefix,
        }
    }

    const ROUTE_TWO_DEFAULTS: &str = "\
Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT
eth0\t00000000\t01C8A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0
wlan0\t00000000\t01C8A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0
";

    #[test]
    fn the_lowest_metric_default_route_wins() {
        // Two default routes is a wired plus a wireless path, not a
        // misconfiguration, and the metric is the statement of which is
        // preferred.
        assert_eq!(
            default_route_iface(ROUTE_TWO_DEFAULTS).as_deref(),
            Some("eth0")
        );
    }

    #[test]
    fn a_table_with_no_default_route_names_no_interface() {
        let t =
            "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n\
                 eth0\t00C8A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(default_route_iface(t), None);
    }

    #[test]
    fn a_second_address_in_the_default_routes_subnet_cannot_answer() {
        // The live case: a ground station on eth0 .201 and wlan0 .202, both in
        // the same /24. It was declared down while serving happily on .201.
        let addrs = [
            a("eth0", "192.168.200.201", 24),
            a("wlan0", "192.168.200.202", 24),
        ];
        assert_eq!(
            unreachable_addrs(&addrs, "eth0"),
            vec!["192.168.200.202".to_string()]
        );
    }

    #[test]
    fn a_second_address_on_its_own_network_is_left_alone() {
        // An access point on 192.168.4.1 is an ordinary second network with its
        // own route. Reporting it would cry wolf on every node running a
        // hotspot, which is most of them.
        let addrs = [
            a("eth0", "192.168.200.201", 24),
            a("wlan0", "192.168.4.1", 24),
        ];
        assert!(unreachable_addrs(&addrs, "eth0").is_empty());
    }

    #[test]
    fn the_real_ground_station_layout_flags_exactly_one_address() {
        // All three addresses the box actually carries.
        let addrs = [
            a("eth0", "192.168.200.201", 24),
            a("wlan0", "192.168.200.202", 24),
            a("wlan0", "192.168.4.1", 24),
        ];
        assert_eq!(
            unreachable_addrs(&addrs, "eth0"),
            vec!["192.168.200.202".to_string()]
        );
    }

    #[test]
    fn the_default_routes_own_address_is_never_called_unreachable() {
        let addrs = [a("eth0", "192.168.200.201", 24)];
        assert!(unreachable_addrs(&addrs, "eth0").is_empty());
    }

    #[test]
    fn nothing_is_condemned_when_no_interface_is_preferred() {
        // With no default route there is no basis for calling one address worse
        // than another, and guessing would take a working node off the air.
        let addrs = [
            a("eth0", "192.168.200.201", 24),
            a("wlan0", "192.168.200.202", 24),
        ];
        assert!(unreachable_addrs(&addrs, "ppp0").is_empty());
    }

    #[test]
    fn the_preferred_address_is_the_one_that_can_answer() {
        let addrs = [
            a("wlan0", "192.168.200.202", 24),
            a("eth0", "192.168.200.201", 24),
        ];
        assert_eq!(
            preferred_addr(&addrs, "eth0").as_deref(),
            Some("192.168.200.201"),
            "order in the list must not decide it"
        );
    }

    #[test]
    fn a_shadowed_address_is_never_offered_as_preferred() {
        // The default-route interface has no address of its own, so the choice
        // falls to the others -- but not to one the rule already condemned.
        let addrs = [
            a("wlan0", "192.168.200.202", 24),
            a("usb0", "192.168.7.1", 24),
        ];
        assert_eq!(
            preferred_addr(&addrs, "eth0").as_deref(),
            Some("192.168.200.202"),
            "nothing is shadowed without a primary to shadow against"
        );
    }

    #[test]
    fn differing_prefixes_use_the_narrower_one() {
        // A /16 on one side covers the /24 on the other, so the kernel still
        // has one route across both and the second address still cannot answer.
        let addrs = [
            a("eth0", "192.168.200.201", 16),
            a("wlan0", "192.168.200.202", 24),
        ];
        assert_eq!(unreachable_addrs(&addrs, "eth0").len(), 1);
    }

    #[test]
    fn a_malformed_address_is_not_condemned_on_a_guess() {
        let addrs = [
            a("eth0", "192.168.200.201", 24),
            a("wlan0", "not-an-address", 24),
        ];
        assert!(unreachable_addrs(&addrs, "eth0").is_empty());
    }
}
