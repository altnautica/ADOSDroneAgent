//! GET /api/v1/ground-station/network/ethernet — ethernet profile + live link.

use super::*;

/// `GET .../network/ethernet` → the discovered ethernet connection name plus the
/// live link legs. 404s on a drone.
///
/// `connection_name` is the active ethernet NM profile's name (e.g.
/// `"netplan-eth0"`) from a read-only `nmcli` connection list, `null` only when
/// no NM-managed ethernet profile exists. `link`, `speed_mbps`, `current_ip` and
/// `current_gateway` are read live from the board's wired port, resolved by
/// device class (see `wired_link`); every leg is `null` when the board has no
/// wired port. The configured legs (`mode`, `ip`, `gateway`, `dns`) stay `null`:
/// nothing here reads the saved profile's settings, and a default would be an
/// assertion ("DHCP is configured") rather than a reading.
pub async fn get_network_ethernet() -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    let wired = wired_link().await;
    let iface = wired
        .as_ref()
        .map(|w| w.iface.clone())
        .unwrap_or_else(|| ETH_IFACE_FALLBACK.to_string());
    let connection_name = discover_primary_connection_name(&iface)
        .await
        .map(Value::String)
        .unwrap_or(Value::Null);
    Json(ethernet_body(connection_name, wired.as_ref())).into_response()
}

/// Compose the `.../network/ethernet` body.
pub(super) fn ethernet_body(connection_name: Value, wired: Option<&WiredLink>) -> Value {
    json!({
        "connection_name": connection_name,
        "mode": Value::Null,
        "ip": Value::Null,
        "gateway": Value::Null,
        "dns": Value::Null,
        "link": wired.and_then(|w| w.link),
        "speed_mbps": wired.and_then(|w| w.speed_mbps),
        "current_ip": wired.and_then(|w| w.ip.clone()),
        "current_gateway": wired.and_then(|w| w.gateway.clone()),
    })
}

/// The `ethernet` leg of the aggregate network view:
/// `{available, iface, link, speed_mbps, ip, gateway}` for the board's wired
/// port, or `null` when the board has none.
pub(super) async fn ethernet_view() -> Value {
    match wired_link().await {
        None => Value::Null,
        Some(w) => json!({
            "available": true,
            "iface": w.iface,
            "link": w.link,
            "speed_mbps": w.speed_mbps,
            "ip": w.ip,
            "gateway": w.gateway,
        }),
    }
}

/// The live state of the board's wired port.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct WiredLink {
    pub(super) iface: String,
    /// Carrier present. `None` when the kernel would not say.
    pub(super) link: Option<bool>,
    /// Negotiated speed; `None` without a link or when the driver reports none.
    pub(super) speed_mbps: Option<u32>,
    pub(super) ip: Option<String>,
    pub(super) gateway: Option<String>,
}

/// Read the first physical wired port (by device class, never by name) and
/// its carrier, speed, IPv4 address and default gateway. `None` when the board
/// has no wired port.
pub(super) async fn wired_link() -> Option<WiredLink> {
    let iface = crate::probe::offload(|| ados_protocol::netif::list_wired().into_iter().next())
        .await
        .flatten()?;
    let sys = Path::new("/sys/class/net").join(&iface);
    let (link, speed_mbps) = crate::probe::offload(move || (read_carrier(&sys), read_speed(&sys)))
        .await
        .unwrap_or((None, None));
    let addr_args = ["-4", "-o", "addr", "show", "dev", iface.as_str()];
    let route_args = ["-4", "route", "show", "default", "dev", iface.as_str()];
    let (addr, route) = tokio::join!(
        crate::probe::capture("ip", &addr_args, crate::probe::PROBE_TIMEOUT),
        crate::probe::capture("ip", &route_args, crate::probe::PROBE_TIMEOUT),
    );
    Some(WiredLink {
        ip: parse_inet_addr(addr.text()),
        gateway: parse_default_via(route.text()),
        iface,
        link,
        speed_mbps,
    })
}

/// Carrier from sysfs. The `carrier` file refuses to read while the interface
/// is administratively down, which is then reported as no link.
pub(super) fn read_carrier(sys: &Path) -> Option<bool> {
    match std::fs::read_to_string(sys.join("carrier")) {
        Ok(v) => match v.trim() {
            "1" => Some(true),
            "0" => Some(false),
            _ => None,
        },
        Err(_) => std::fs::read_to_string(sys.join("operstate"))
            .ok()
            .filter(|s| s.trim() == "down")
            .map(|_| false),
    }
}

/// Negotiated speed in Mb/s; the kernel reports `-1` (or refuses) with no link.
pub(super) fn read_speed(sys: &Path) -> Option<u32> {
    std::fs::read_to_string(sys.join("speed"))
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|&s| s > 0)
        .and_then(|s| u32::try_from(s).ok())
}

/// The first IPv4 address in `ip -4 -o addr show` output, without its prefix.
pub(super) fn parse_inet_addr(text: &str) -> Option<String> {
    let mut words = text.split_whitespace();
    while let Some(w) = words.next() {
        if w == "inet" {
            return words
                .next()
                .and_then(|a| a.split('/').next())
                .map(str::to_string);
        }
    }
    None
}

/// The gateway of the first default route in `ip -4 route show default` output.
pub(super) fn parse_default_via(text: &str) -> Option<String> {
    let mut words = text.split_whitespace();
    while let Some(w) = words.next() {
        if w == "via" {
            return words.next().map(str::to_string);
        }
    }
    None
}

/// The interface the NM connection discovery falls back to when the board
/// reports no wired port, mirroring the Python `EthernetManager` default.
pub(super) const ETH_IFACE_FALLBACK: &str = "eth0";

/// Discover the primary ethernet NM connection NAME, mirroring the Python
/// `_discover_primary_connection`. Reads the saved + active NM connection lists
/// with a read-only `nmcli` and picks the primary name. Returns `None` when
/// `nmcli` is absent / errors / lists no ethernet profile — the same `null` the
/// Python view reports on a non-NM box.
pub(super) async fn discover_primary_connection_name(interface: &str) -> Option<String> {
    // The saved and active listings are two independent `nmcli` runs, so they go
    // on the wire together. `nmcli` is the slowest probe on this surface even
    // when healthy, and running them in sequence doubled this route's floor for
    // no reason: neither listing is an input to the other.
    //
    // The field list is bound OUTSIDE the `join!`: an array literal written in a
    // macro argument is a temporary of the expansion's own `let`, dropped before
    // the poll loop the futures borrowing it are driven by.
    let fields: &[&str] = &["NAME", "TYPE", "DEVICE"];
    let (saved, active) = tokio::join!(
        nmcli_connections(fields, false),
        nmcli_connections(fields, true),
    );
    pick_primary_connection_name(&saved, &active, interface)
}

/// Pick the primary ethernet connection name from the parsed saved + active
/// terse rows, mirroring the Python `_discover_primary_connection` precedence:
/// an ACTIVE 802-3-ethernet connection on the interface (or with no device
/// pinned) wins, else a saved ethernet connection pinned to the interface, else
/// the first saved ethernet connection of any device, else `None`.
pub(super) fn pick_primary_connection_name(
    saved_rows: &[Vec<String>],
    active_rows: &[Vec<String>],
    interface: &str,
) -> Option<String> {
    // (name, device) of every saved 802-3-ethernet connection.
    let saved: Vec<(&str, &str)> = saved_rows
        .iter()
        .filter(|row| row.get(1).map(String::as_str) == Some("802-3-ethernet"))
        .map(|row| {
            let name = row.first().map(String::as_str).unwrap_or("");
            let dev = row.get(2).map(String::as_str).unwrap_or("");
            (name, dev)
        })
        .collect();

    // The names of the active connections (the `--active` view).
    let active_names: std::collections::HashSet<&str> = active_rows
        .iter()
        .filter_map(|row| row.first().map(String::as_str))
        .filter(|name| !name.is_empty())
        .collect();

    // An active ethernet connection on the interface (or with no device pinned).
    for (name, dev) in &saved {
        if active_names.contains(name) && (*dev == interface || dev.is_empty()) {
            return Some((*name).to_string());
        }
    }
    // A saved ethernet connection pinned to the interface.
    for (name, dev) in &saved {
        if *dev == interface {
            return Some((*name).to_string());
        }
    }
    // Else the first saved ethernet connection of any device.
    saved.first().map(|(name, _dev)| (*name).to_string())
}

/// Run a read-only `nmcli -t -f <fields> connection show [--active]` and parse
/// the terse rows. Each row is truncated to `fields.len()` columns. An absent
/// `nmcli` / a non-zero exit / a spawn error / a timeout all yield an empty list,
/// so the caller degrades to the no-connection `null`.
///
/// Bounded by `SLOW_PROBE_TIMEOUT` rather than the default: `nmcli` re-reads
/// every connection profile and takes hundreds of milliseconds on a healthy box,
/// so the short bound would turn a working listing into an empty one.
pub(crate) async fn nmcli_connections(fields: &[&str], active: bool) -> Vec<Vec<String>> {
    let mut args = vec!["-t", "-f"];
    let field_spec = fields.join(",");
    args.push(&field_spec);
    args.push("connection");
    args.push("show");
    if active {
        args.push("--active");
    }
    parse_nmcli_terse(
        crate::probe::capture("nmcli", &args, crate::probe::SLOW_PROBE_TIMEOUT)
            .await
            .text(),
        fields.len(),
    )
}

/// Parse `nmcli -t` (terse) multi-line output into rows, skipping blank lines and
/// keeping only rows with at least `fields` columns (each truncated to `fields`).
pub(super) fn parse_nmcli_terse(text: &str, fields: usize) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parts = parse_nmcli_terse_line(line);
        if parts.len() >= fields {
            rows.push(parts.into_iter().take(fields).collect());
        }
    }
    rows
}

/// Split one `nmcli -t` terse line into fields, honoring `\:` and `\\` escapes.
/// An odd trailing backslash is treated as a literal backslash, matching the
/// Python `_parse_nmcli_terse_fields` `i + 1 < len(line)` guard.
pub(super) fn parse_nmcli_terse_line(line: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\\' && i + 1 < chars.len() {
            buf.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if ch == ':' {
            parts.push(std::mem::take(&mut buf));
            i += 1;
            continue;
        }
        buf.push(ch);
        i += 1;
    }
    parts.push(buf);
    parts
}
