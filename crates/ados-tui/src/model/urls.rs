//! URL and host helpers for the reach links, plus JSON field readers.

use super::*;

/// Derive the browser viewer URL from a WHEP URL: strip a trailing `/whep`,
/// then end with `/`. A relative WHEP URL lives on the agent's own front, where
/// the viewer is the cockpit link the status already carries in `access_urls`,
/// so it yields `None` rather than a host-less path.
pub fn viewer_url_from_whep(whep_url: Option<&str>) -> Option<String> {
    let whep = whep_url?;
    if whep.is_empty() || whep.starts_with('/') {
        return None;
    }
    let mut base = whep.trim_end_matches('/').to_string();
    if let Some(stripped) = base.strip_suffix("/whep") {
        base = stripped.to_string();
    }
    Some(format!("{base}/"))
}

/// The lowercased host of a URL, with the scheme, path, and port stripped.
pub(super) fn url_host(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host_port = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let host = if let Some(rest) = host_port.strip_prefix('[') {
        // Bracketed IPv6 literal, e.g. `[::1]:8080`.
        rest.split(']').next().unwrap_or(rest)
    } else {
        host_port
            .rsplit_once(':')
            .map(|(host, _)| host)
            .unwrap_or(host_port)
    };
    host.to_ascii_lowercase()
}

/// De-duplicate rows by URL (first wins) and drop loopback rows once any
/// routable row exists.
pub(super) fn dedup_drop_loopback(rows: Vec<LinkRow>) -> Vec<LinkRow> {
    let mut out: Vec<LinkRow> = Vec::new();
    for row in rows {
        if !out.iter().any(|r| r.url == row.url) {
            out.push(row);
        }
    }
    if out.iter().any(|r| !r.loopback) {
        out.retain(|r| !r.loopback);
    }
    out
}

pub(super) fn is_loopback_host(host: &str) -> bool {
    host == "localhost" || host == "::1" || host.starts_with("127.")
}

pub(super) fn is_ipv4(host: &str) -> bool {
    let octets: Vec<&str> = host.split('.').collect();
    octets.len() == 4
        && octets.iter().all(|part| {
            !part.is_empty()
                && part.chars().all(|c| c.is_ascii_digit())
                && part.parse::<u16>().is_ok_and(|n| n <= 255)
        })
}

/// Reach priority for a URL, lowest first: mDNS `.local` (0), LAN IP (1),
/// other hostname / tunnel (2), loopback (3). The operator is almost always
/// on another machine, so `localhost` is the least useful and sorts last.
pub fn reach_rank(url: &str) -> u8 {
    let host = url_host(url);
    if is_loopback_host(&host) {
        3
    } else if host.ends_with(".local") {
        0
    } else if is_ipv4(&host) {
        1
    } else {
        2
    }
}

/// True when the URL points at the local loopback (useless over SSH).
pub fn is_loopback_url(url: &str) -> bool {
    is_loopback_host(&url_host(url))
}

/// Where the agent persists the access-point passphrase.
pub(super) const AP_PASSPHRASE_PATH: &str = "/etc/ados/ap-passphrase";

/// The AP passphrase, or empty when it cannot be read.
///
/// Read straight off disk rather than through the status API: this view is the
/// operator's fallback when the network is the thing that is broken, and the
/// file is local and 0600. Empty rather than a placeholder — a wrong
/// passphrase looks exactly like a broken radio to whoever is typing it.
pub(super) fn read_ap_passphrase() -> String {
    std::fs::read_to_string(AP_PASSPHRASE_PATH)
        .map(|b| b.trim().to_string())
        .unwrap_or_default()
}

pub(super) fn s(v: &Value, key: &str, default: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or(default)
        .to_string()
}

/// A non-empty string field as `Option<String>`.
pub(super) fn opt_str(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The port number in a URL authority, e.g. `8765` from `ws://host:8765/`.
pub(super) fn port_of(url: &str) -> Option<u16> {
    let authority = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    let after = authority.rsplit(']').next().unwrap_or(authority); // past an IPv6 literal
    after.rsplit_once(':').and_then(|(_, p)| p.parse().ok())
}

/// Replace the host in a URL, preserving the scheme, port, and path. Used to
/// project a service URL (advertised on one best host) onto every reachable
/// host so the operator sees both the mDNS name and the LAN IP.
pub(super) fn swap_host(url: &str, host: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (Some(s), r),
        None => (None, url),
    };
    let (authority, tail) = match rest.find(['/', '?', '#']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    // Keep the `:port` suffix of the old authority (skip an IPv6 literal).
    let port = authority
        .rsplit(']')
        .next()
        .unwrap_or(authority)
        .rsplit_once(':')
        .map(|(_, p)| format!(":{p}"))
        .unwrap_or_default();
    match scheme {
        Some(scheme) => format!("{scheme}://{host}{port}{tail}"),
        None => format!("{host}{port}{tail}"),
    }
}

/// The scheme + authority of a URL, dropping the path/query/fragment, e.g.
/// `http://host:8080/setup` → `http://host:8080`. Turns the per-host setup URLs
/// into the bare base address to paste into Mission Control's Add-a-Node card.
pub(super) fn base_url(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (Some(s), r),
        None => (None, url),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    match scheme {
        Some(scheme) => format!("{scheme}://{authority}"),
        None => authority.to_string(),
    }
}
