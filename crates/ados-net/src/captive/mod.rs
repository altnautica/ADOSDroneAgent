//! First-boot captive portal: a DNS and HTTP responder for the setup AP.
//!
//! When a phone joins the ground-node access point, its OS probes a vendor
//! connectivity URL and opens a sign-in view when the answer is not the one
//! the open internet gives. This responder:
//!
//! 1. answers DNS `A` queries for the vendor probe hostnames (and the node's
//!    own setup names) with the AP gateway address, and NXDOMAIN for every
//!    other name;
//! 2. answers every HTTP request on port 80 with a `302` to the setup webapp
//!    on the agent's API port.
//!
//! Every probe gets the redirect rather than its expected answer: a `204` on
//! `/generate_204` tells Android the network is validated and suppresses the
//! sign-in sheet. The redirect target is on the API port, never port 80, so a
//! browser that follows it lands on the wizard instead of looping here.
//!
//! The service stands down (exit 0, which the unit treats as final) once setup
//! is complete, when `ados-dnsmasq-gs` is the node's DNS owner, or when port 53
//! is already taken; see [`run`].

use std::net::Ipv4Addr;
use std::path::Path;
use std::time::Duration;

mod run;

pub use run::{run, run_on, serve};

/// Sentinel written once the node is paired; the portal has nothing to do
/// after that. The unit's `ConditionPathExists` names the same file.
pub const SETUP_COMPLETE_PATH: &str = "/var/lib/ados/setup-complete";

/// The ground-station dnsmasq config. Its presence (with the unit enabled)
/// means `ados-dnsmasq-gs` owns DNS on the AP interface.
pub const DNSMASQ_GS_CONF: &str = "/etc/ados/dnsmasq-gs.conf";

/// The AP gateway: every captured name resolves here.
pub const AP_GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 1);

/// TTL of a captured-name answer, in seconds.
pub const ANSWER_TTL_S: u32 = 60;

/// The API port the redirect targets when the config cannot be read.
pub const DEFAULT_API_PORT: u16 = 8080;

/// DNS and HTTP ports the service binds on the node.
pub const DNS_PORT: u16 = 53;
pub const HTTP_PORT: u16 = 80;

/// Hostnames answered with [`AP_GATEWAY_IP`]. Matched lower-case.
pub const CAPTURED_HOSTS: &[&str] = &[
    // Android and Chrome OS.
    "connectivitycheck.gstatic.com",
    "www.google.com",
    "www.gstatic.com",
    // iOS and macOS.
    "captive.apple.com",
    "www.apple.com",
    // Windows.
    "www.msftconnecttest.com",
    "dns.msftncsi.com",
    "www.msftncsi.com",
    // Samsung.
    "connectivitycheck.samsung.com",
    // The node itself.
    "setup.ados.local",
    "ados.local",
];

/// The OS connectivity probe paths. Every path gets the same redirect; a hit
/// on one of these is logged because it proves an OS saw the portal.
pub const PROBE_PATHS: &[&str] = &[
    "/generate_204",
    "/gen_204",
    "/hotspot-detect.html",
    "/library/test/success.html",
    "/connecttest.txt",
    "/ncsi.txt",
    "/success.txt",
];

/// One parsed DNS question: `(txid, qname, qtype, qclass)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub txid: u16,
    pub qname: String,
    pub qtype: u16,
    pub qclass: u16,
}

/// Parse the first question of a DNS query. `None` on malformed input: fewer
/// than 12 header bytes, no question, a compression pointer in the name, or a
/// truncated name or type/class.
pub fn parse_question(data: &[u8]) -> Option<Question> {
    if data.len() < 12 {
        return None;
    }
    let txid = u16::from_be_bytes([data[0], data[1]]);
    let qdcount = u16::from_be_bytes([data[4], data[5]]);
    if qdcount < 1 {
        return None;
    }
    let mut i = 12;
    let mut labels: Vec<String> = Vec::new();
    while i < data.len() {
        let len = data[i] as usize;
        if len == 0 {
            i += 1;
            break;
        }
        if len & 0xC0 != 0 {
            return None;
        }
        i += 1;
        if i + len > data.len() {
            return None;
        }
        labels.push(ascii_lossy(&data[i..i + len]));
        i += len;
    }
    if i + 4 > data.len() {
        return None;
    }
    Some(Question {
        txid,
        qname: labels.join(".").to_lowercase(),
        qtype: u16::from_be_bytes([data[i], data[i + 1]]),
        qclass: u16::from_be_bytes([data[i + 2], data[i + 3]]),
    })
}

/// Bytes as ASCII, dropping anything outside it.
fn ascii_lossy(bytes: &[u8]) -> String {
    bytes
        .iter()
        .filter(|b| b.is_ascii())
        .map(|&b| b as char)
        .collect()
}

/// `name` in DNS wire form (length-prefixed labels, zero terminator).
fn encode_name(name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(name.len() + 2);
    for label in name.trim_end_matches('.').split('.') {
        let ascii: Vec<u8> = label.bytes().filter(u8::is_ascii).collect();
        out.push(ascii.len() as u8);
        out.extend_from_slice(&ascii);
    }
    out.push(0);
    out
}

fn header(txid: u16, flags: u16, ancount: u16) -> Vec<u8> {
    let mut h = Vec::with_capacity(12);
    for word in [txid, flags, 1, ancount, 0, 0] {
        h.extend_from_slice(&word.to_be_bytes());
    }
    h
}

/// A single-`A`-record answer for `qname` pointing at `ip` (flags `0x8180`,
/// a `0xC00C` name pointer, TTL [`ANSWER_TTL_S`]).
pub fn build_answer(txid: u16, qname: &str, ip: Ipv4Addr) -> Vec<u8> {
    let mut out = header(txid, 0x8180, 1);
    out.extend(encode_name(qname));
    out.extend_from_slice(&[0, 1, 0, 1]); // A, IN
    out.extend_from_slice(&0xC00Cu16.to_be_bytes());
    out.extend_from_slice(&[0, 1, 0, 1]); // A, IN
    out.extend_from_slice(&ANSWER_TTL_S.to_be_bytes());
    out.extend_from_slice(&4u16.to_be_bytes());
    out.extend_from_slice(&ip.octets());
    out
}

/// An NXDOMAIN reply echoing the question (flags `0x8183`).
pub fn build_nxdomain(txid: u16, qname: &str, qtype: u16, qclass: u16) -> Vec<u8> {
    let mut out = header(txid, 0x8183, 0);
    out.extend(encode_name(qname));
    out.extend_from_slice(&qtype.to_be_bytes());
    out.extend_from_slice(&qclass.to_be_bytes());
    out
}

/// The reply to one DNS datagram, or `None` to drop a malformed one. Only an
/// `A`/`IN` query for a captured host is answered; everything else is
/// NXDOMAIN.
pub fn dns_reply(data: &[u8]) -> Option<Vec<u8>> {
    let q = parse_question(data)?;
    if q.qtype == 1 && q.qclass == 1 && CAPTURED_HOSTS.contains(&q.qname.as_str()) {
        tracing::debug!(qname = %q.qname, "dns_answered");
        Some(build_answer(q.txid, &q.qname, AP_GATEWAY_IP))
    } else {
        tracing::debug!(qname = %q.qname, qtype = q.qtype, "dns_nxdomain");
        Some(build_nxdomain(q.txid, &q.qname, q.qtype, q.qclass))
    }
}

/// The setup webapp every client is sent to.
pub fn setup_url(api_port: u16) -> String {
    format!("http://{AP_GATEWAY_IP}:{api_port}/")
}

/// The full HTTP/1.0 response to a request with `method` and `path`: a `302`
/// to [`setup_url`] for `GET` (with a small HTML body) and `HEAD` (headers
/// only), `501` for any other method.
pub fn http_response(method: &str, path: &str, api_port: u16) -> Vec<u8> {
    let target = setup_url(api_port);
    match method {
        "GET" | "HEAD" => {
            if PROBE_PATHS.contains(&path.split('?').next().unwrap_or("")) {
                tracing::debug!(path, "captive_probe_redirected");
            }
            let body = format!(
                "<!doctype html><title>ADOS setup</title><a href=\"{target}\">Open ADOS setup</a>\n"
            );
            let mut out = format!(
                "HTTP/1.0 302 Found\r\nLocation: {target}\r\n\
                 Content-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
                 Cache-Control: no-store\r\n\r\n",
                body.len()
            )
            .into_bytes();
            if method == "GET" {
                out.extend_from_slice(body.as_bytes());
            }
            out
        }
        _ => b"HTTP/1.0 501 Not Implemented\r\nContent-Length: 0\r\n\r\n".to_vec(),
    }
}

/// Why the portal should not run, or `None` when it should: setup is already
/// complete (`sentinel` exists), or `dnsmasq_owns_dns` reports the node's own
/// DNS server is the intended owner of port 53.
pub fn stand_down_reason(sentinel: &Path, dnsmasq_owns_dns: bool) -> Option<&'static str> {
    if sentinel.exists() {
        return Some("setup already complete");
    }
    if dnsmasq_owns_dns {
        return Some("ados-dnsmasq-gs is the intended DNS owner on this node");
    }
    None
}

/// True when `ados-dnsmasq-gs` is configured and enabled. On a ground station
/// it owns `wlan0:53`; a portal bound to `0.0.0.0:53` first would win the boot
/// race and keep it from ever starting (Linux refuses the interface bind once
/// the wildcard is taken), so the portal yields without binding.
pub async fn dnsmasq_gs_owns_dns() -> bool {
    if !Path::new(DNSMASQ_GS_CONF).exists() {
        return false;
    }
    ados_proc::run::run(
        "/bin/systemctl",
        &["is-enabled", "--quiet", "ados-dnsmasq-gs.service"],
        &[],
        Duration::from_secs(2),
    )
    .await
    .success()
}

/// The agent's REST port from the config at `config_path` (schema defaults
/// apply), or [`DEFAULT_API_PORT`] when it cannot be read.
pub fn api_port(config_path: &Path) -> u16 {
    match ados_config::schema::load_slice::<u16>(config_path, "/api/rest/port") {
        Ok(port) => port,
        Err(error) => {
            tracing::warn!(%error, "captive_api_port_unreadable");
            DEFAULT_API_PORT
        }
    }
}

#[cfg(test)]
mod tests;
