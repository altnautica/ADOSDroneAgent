//! The portal's lifecycle and its two listeners.

use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

use super::{
    api_port, dns_reply, dnsmasq_gs_owns_dns, http_response, stand_down_reason, CAPTURED_HOSTS,
    DNS_PORT, HTTP_PORT, SETUP_COMPLETE_PATH,
};

/// Largest request head read from an HTTP client.
const MAX_REQUEST_HEAD: usize = 8 * 1024;
/// How long a client gets to send its request head.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Pause after a socket error before the next receive or accept, so a
/// persistent error cannot spin the loop.
const SOCKET_ERROR_PAUSE: Duration = Duration::from_millis(200);

/// The service: stand down when there is nothing to serve, otherwise bind
/// `0.0.0.0:53` and `0.0.0.0:80` and answer until SIGTERM or SIGINT. Every
/// path exits 0; the unit treats that as final and only a crash restarts it.
pub async fn run() -> ExitCode {
    let sentinel = Path::new(SETUP_COMPLETE_PATH);
    let dnsmasq_owns = !sentinel.exists() && dnsmasq_gs_owns_dns().await;
    if let Some(reason) = stand_down_reason(sentinel, dnsmasq_owns) {
        tracing::info!(reason, "captive_dns_inactive");
        return ExitCode::SUCCESS;
    }
    let config =
        std::env::var("ADOS_CONFIG").unwrap_or_else(|_| crate::config::CONFIG_YAML.to_string());
    let port = api_port(Path::new(&config));
    run_on(
        SocketAddr::from(([0, 0, 0, 0], DNS_PORT)),
        SocketAddr::from(([0, 0, 0, 0], HTTP_PORT)),
        port,
        stop_signal(),
    )
    .await
}

/// Bind DNS on `dns` and HTTP on `http`, then [`serve`] until `shutdown`
/// resolves. A DNS bind failure (another server owns the port) exits 0 without
/// serving: whoever owns it already answers a joining phone. An HTTP bind
/// failure leaves DNS running alone.
pub async fn run_on(
    dns: SocketAddr,
    http: SocketAddr,
    api_port: u16,
    shutdown: impl Future<Output = ()>,
) -> ExitCode {
    let udp = match UdpSocket::bind(dns).await {
        Ok(sock) => sock,
        Err(error) => {
            tracing::info!(port = dns.port(), %error, "captive_dns_skipped");
            return ExitCode::SUCCESS;
        }
    };
    tracing::info!(port = dns.port(), "captive_dns_bound");
    let tcp = match TcpListener::bind(http).await {
        Ok(listener) => {
            tracing::info!(
                port = http.port(),
                redirect = %super::setup_url(api_port),
                "captive_http_bound"
            );
            Some(listener)
        }
        Err(error) => {
            tracing::info!(port = http.port(), %error, "captive_http_skipped");
            None
        }
    };
    serve(udp, tcp, api_port, shutdown).await;
    ExitCode::SUCCESS
}

/// Answer DNS on `udp` and redirect HTTP on `tcp` (when bound) until
/// `shutdown` resolves.
pub async fn serve(
    udp: UdpSocket,
    tcp: Option<TcpListener>,
    api_port: u16,
    shutdown: impl Future<Output = ()>,
) {
    tracing::info!(captured = CAPTURED_HOSTS.len(), "captive_dns_service_ready");
    tokio::select! {
        () = dns_loop(&udp) => {}
        () = http_loop(tcp, api_port) => {}
        () = shutdown => tracing::info!("captive_dns_signal_stop"),
    }
    tracing::info!("captive_dns_service_stopped");
}

async fn dns_loop(udp: &UdpSocket) {
    let mut buf = [0u8; 4096];
    loop {
        let (n, peer) = match udp.recv_from(&mut buf).await {
            Ok(got) => got,
            Err(error) => {
                tracing::debug!(%error, "dns_recv_failed");
                tokio::time::sleep(SOCKET_ERROR_PAUSE).await;
                continue;
            }
        };
        let Some(reply) = dns_reply(&buf[..n]) else {
            continue;
        };
        if let Err(error) = udp.send_to(&reply, peer).await {
            tracing::debug!(%error, "dns_send_failed");
        }
    }
}

async fn http_loop(tcp: Option<TcpListener>, api_port: u16) {
    let Some(listener) = tcp else {
        return std::future::pending().await;
    };
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(handle_client(stream, api_port));
            }
            Err(error) => {
                tracing::debug!(%error, "http_accept_failed");
                tokio::time::sleep(SOCKET_ERROR_PAUSE).await;
            }
        }
    }
}

/// Read one request head, answer it, close.
async fn handle_client(mut stream: TcpStream, api_port: u16) {
    let head = match tokio::time::timeout(REQUEST_TIMEOUT, read_head(&mut stream)).await {
        Ok(Ok(head)) => head,
        _ => return,
    };
    let line = head.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return;
    };
    let response = http_response(method, path, api_port);
    let _ = stream.write_all(&response).await;
    let _ = stream.shutdown().await;
}

/// Bytes up to the blank line that ends a request head (or the size cap).
async fn read_head(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut head = Vec::with_capacity(512);
    let mut chunk = [0u8; 1024];
    while head.len() < MAX_REQUEST_HEAD {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(&chunk[..n]);
        if head.windows(4).any(|w| w == b"\r\n\r\n") || head.windows(2).any(|w| w == b"\n\n") {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

/// Resolves on SIGTERM or SIGINT.
async fn stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
