//! LAN address discovery, TCP readiness probe, and the minimal loopback HTTP GET.

use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Discover the SBC's outbound LAN IPv4 by opening a UDP socket toward a public
/// address (no packet is sent — a UDP connect is just a routing-table lookup)
/// and reading the bound local address. mediamtx ICE auto-discovery sometimes
/// only finds 127.0.0.1 on a bench rig, so this is forced into the config as a
/// WebRTC host candidate. Filters loopback + link-local. Best-effort: returns
/// empty on any error.
pub fn detect_lan_ips() -> Vec<String> {
    let mut ips = Vec::new();
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                let ip = addr.ip().to_string();
                if ip != "127.0.0.1" && !ip.starts_with("169.254.") {
                    ips.push(ip);
                }
            }
        }
    }
    ips
}

/// Names of physical wired/WiFi interfaces (`e*`/`en*`/`eth*`/`end*`, `w*`/
/// `wl*`/`wlan*`), excluding loopback, virtual, container, and mesh interfaces,
/// read from `/sys/class/net`. Used to scope WebRTC ICE host-candidate
/// gathering to real reachable networks so the offer never carries loopback /
/// IPv6 link-local / docker / mesh candidates that just fail their checks.
/// mediamtx (Pion) re-reads the addresses of these interfaces per WebRTC
/// session, so a node that moves from ethernet to WiFi advertises whichever
/// interface is up. Best-effort: returns empty when `/sys/class/net` is
/// unreadable (mediamtx then falls back to all interfaces).
pub fn physical_lan_interfaces() -> Vec<String> {
    const SKIP: &[&str] = &[
        "lo", "docker", "veth", "br-", "bat", "tap", "tun", "wg", "virbr", "vmnet",
    ];
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if SKIP.iter().any(|p| name.starts_with(p)) {
                    continue;
                }
                if name.starts_with('e') || name.starts_with('w') {
                    names.push(name.to_string());
                }
            }
        }
    }
    names.sort();
    names
}

/// Async TCP-connect probe: poll `(host, port)` at [`RTSP_BIND_PROBE_INTERVAL`]
/// until a connect succeeds or `timeout` elapses. Each probe uses a short
/// connect timeout so a stalled stack does not hold the loop. Used to gate the
/// encoder spawn on the mediamtx RTSP listener actually accepting.
pub async fn wait_for_tcp_port(host: &str, port: u16, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let connect = TcpStream::connect((host, port));
        if let Ok(Ok(stream)) = tokio::time::timeout(API_CONNECT_TIMEOUT, connect).await {
            drop(stream);
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(RTSP_BIND_PROBE_INTERVAL).await;
    }
}

/// Minimal async HTTP/1.1 GET to 127.0.0.1:`port``path`. Returns the response
/// body bytes on a 200, `None` on connection-refused / timeout / non-200 /
/// malformed response. Loopback-only, so `Connection: close` + read-to-EOF is
/// sufficient (no chunked / keep-alive handling).
pub(super) async fn http_get(port: u16, path: &str) -> Option<Vec<u8>> {
    let connect = TcpStream::connect(("127.0.0.1", port));
    let mut stream = tokio::time::timeout(API_CONNECT_TIMEOUT, connect)
        .await
        .ok()?
        .ok()?;

    let request =
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");

    let exchange = async {
        stream.write_all(request.as_bytes()).await.ok()?;
        stream.flush().await.ok()?;
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.ok()?;
        Some(buf)
    };
    let raw = tokio::time::timeout(API_TIMEOUT, exchange).await.ok()??;

    // Split status line + headers from the body on the first CRLFCRLF.
    let sep = find_subslice(&raw, b"\r\n\r\n")?;
    let head = &raw[..sep];
    let body = &raw[sep + 4..];

    // The status line is the first line of the head: "HTTP/1.1 200 OK".
    let status_line_end = find_subslice(head, b"\r\n").unwrap_or(head.len());
    let status_line = std::str::from_utf8(&head[..status_line_end]).ok()?;
    let code: u16 = status_line.split_whitespace().nth(1)?.parse().ok()?;
    if code != 200 {
        return None;
    }
    Some(body.to_vec())
}

/// Find the first index of `needle` in `haystack`.
pub(super) fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}
