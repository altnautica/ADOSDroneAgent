//! Reachability probe for a network camera source.
//!
//! A process that opens an RTSP or HTTP camera while the camera is unreachable
//! gets stuck in the kernel's connect retry: a dropped SYN is retransmitted
//! after 1 s, 3 s, 7 s, 15 s and so on, so a camera that comes back in the
//! middle of that schedule is only seen at the next retransmit, seconds late.
//! A daemon that has to reopen such a source probes it instead: one short TCP
//! connect at a time, and it opens the source only once a probe is accepted.
//!
//! The cadence follows [`probe_delay`]: every [`PROBE_INTERVAL`] for the first
//! [`FAST_PROBE_WINDOW`] after the source went down (a brief drop is seen
//! within one interval of the camera answering), then every
//! [`STEADY_PROBE_INTERVAL`] for as long as it stays down. A dead camera costs
//! one SYN per steady interval, and the probe never gives up.
//!
//! Name resolution is not part of the connect bound: a hostname is resolved
//! first under its own, longer bound ([`RESOLVE_TIMEOUT`]), so a slow resolver
//! delays a probe rather than failing every one.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::time::Instant;

/// The fast probe cadence, and the bound on each probe's connect.
pub const PROBE_INTERVAL: Duration = Duration::from_millis(250);

/// How long after the source went down the probe keeps the fast cadence.
pub const FAST_PROBE_WINDOW: Duration = Duration::from_secs(5);

/// The probe cadence once the source has been down past the fast window.
pub const STEADY_PROBE_INTERVAL: Duration = Duration::from_secs(3);

/// The bound on resolving a source's hostname, separate from the connect's.
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

/// The pause before the next probe of a source that has been down for
/// `down_for`.
pub fn probe_delay(down_for: Duration) -> Duration {
    if down_for < FAST_PROBE_WINDOW {
        PROBE_INTERVAL
    } else {
        STEADY_PROBE_INTERVAL
    }
}

/// Where a network source's connection goes: its host and TCP port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceAddr {
    pub host: String,
    pub port: u16,
}

impl SourceAddr {
    /// The host and port of an `rtsp://`, `rtsps://`, `http://` or `https://`
    /// URL, with the scheme's default port when the URL names none. Any other
    /// URL (or one with no host) is `None`: there is nothing to probe.
    pub fn from_url(url: &str) -> Option<Self> {
        let (scheme, rest) = url.trim().split_once("://")?;
        let default_port = match scheme.to_ascii_lowercase().as_str() {
            "rtsp" => 554,
            "rtsps" => 322,
            "http" => 80,
            "https" => 443,
            _ => return None,
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let host_port = authority
            .rsplit_once('@')
            .map_or(authority, |(_, after)| after);
        let (host, port) = if let Some(bracketed) = host_port.strip_prefix('[') {
            let (host, after) = bracketed.split_once(']')?;
            match after.strip_prefix(':') {
                Some(port) => (host, port.parse().ok()?),
                None if after.is_empty() => (host, default_port),
                None => return None,
            }
        } else {
            match host_port.rsplit_once(':') {
                Some((host, port)) => (host, port.parse().ok()?),
                None => (host_port, default_port),
            }
        };
        (!host.is_empty()).then(|| Self {
            host: host.to_string(),
            port,
        })
    }

    /// The socket addresses to probe: an IP literal as is, a hostname resolved
    /// under [`RESOLVE_TIMEOUT`] (empty when it does not resolve in time).
    async fn resolve(&self) -> Vec<SocketAddr> {
        if let Ok(ip) = self.host.parse::<IpAddr>() {
            return vec![SocketAddr::new(ip, self.port)];
        }
        match tokio::time::timeout(
            RESOLVE_TIMEOUT,
            tokio::net::lookup_host((self.host.as_str(), self.port)),
        )
        .await
        {
            Ok(Ok(addrs)) => addrs.collect(),
            _ => Vec::new(),
        }
    }

    /// One probe: does the source accept a TCP connection, each connect
    /// bounded by [`PROBE_INTERVAL`]? Resolving a hostname comes first and is
    /// bounded on its own. The connection is closed at once.
    pub async fn reachable(&self) -> bool {
        for addr in self.resolve().await {
            if matches!(
                tokio::time::timeout(PROBE_INTERVAL, TcpStream::connect(addr)).await,
                Ok(Ok(_))
            ) {
                return true;
            }
        }
        false
    }

    /// Resolve once the source accepts a connection. The caller has just seen
    /// a probe fail, so the first probe here waits one [`probe_delay`]; the
    /// cadence is measured from `down_since`, when the source was first seen
    /// down. Never gives up; drop the future to stop probing.
    pub async fn wait_reachable(&self, down_since: Instant) {
        loop {
            tokio::time::sleep(probe_delay(down_since.elapsed())).await;
            if self.reachable().await {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(host: &str, port: u16) -> Option<SourceAddr> {
        Some(SourceAddr {
            host: host.to_string(),
            port,
        })
    }

    #[test]
    fn a_source_url_resolves_to_its_host_and_port() {
        assert_eq!(
            SourceAddr::from_url("rtsp://192.0.2.10:8554/cam"),
            addr("192.0.2.10", 8554)
        );
        assert_eq!(
            SourceAddr::from_url("rtsp://cam.local/stream"),
            addr("cam.local", 554)
        );
        assert_eq!(
            SourceAddr::from_url("RTSPS://cam.local/s"),
            addr("cam.local", 322)
        );
        assert_eq!(
            SourceAddr::from_url("rtsp://user:p@ss:w0rd@10.0.0.5:8554/live?x=1"),
            addr("10.0.0.5", 8554)
        );
        assert_eq!(
            SourceAddr::from_url("http://[fe80::1]:8080/mjpeg"),
            addr("fe80::1", 8080)
        );
        assert_eq!(SourceAddr::from_url("https://[::1]"), addr("::1", 443));
        assert_eq!(SourceAddr::from_url(" http://cam#frag"), addr("cam", 80));
    }

    #[test]
    fn anything_but_a_network_url_has_nothing_to_probe() {
        for url in [
            "/dev/video0",
            "framebus://front",
            "udp://1.2.3.4:5600",
            "rtsp://",
            "rtsp:///path",
            "rtsp://host:notaport/x",
            "rtsp://[::1]x/",
        ] {
            assert_eq!(SourceAddr::from_url(url), None, "{url}");
        }
    }

    /// Down (nothing listening) the probe fails; the waiter keeps probing and
    /// resolves within about one interval of a listener appearing.
    #[tokio::test]
    async fn the_waiter_resolves_within_one_interval_of_the_source_answering() {
        let parked = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = parked.local_addr().unwrap().port();
        drop(parked);
        let source = SourceAddr {
            host: "127.0.0.1".into(),
            port,
        };
        assert!(!source.reachable().await);

        let waiter = tokio::spawn({
            let source = source.clone();
            async move { source.wait_reachable(tokio::time::Instant::now()).await }
        });
        tokio::time::sleep(PROBE_INTERVAL * 3).await;
        assert!(!waiter.is_finished(), "nothing is listening yet");

        let _listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .unwrap();
        let back = tokio::time::Instant::now();
        tokio::time::timeout(PROBE_INTERVAL * 3, waiter)
            .await
            .expect("the waiter sees the source within an interval")
            .unwrap();
        assert!(back.elapsed() <= PROBE_INTERVAL * 2, "{:?}", back.elapsed());
        assert!(source.reachable().await);
    }

    /// Fast for the first seconds after the source went down, then a fixed
    /// steady cadence inside 2-5 s for as long as it stays down.
    #[test]
    fn the_probe_backs_off_to_a_steady_cadence() {
        assert_eq!(probe_delay(Duration::ZERO), PROBE_INTERVAL);
        assert_eq!(
            probe_delay(FAST_PROBE_WINDOW - Duration::from_millis(1)),
            PROBE_INTERVAL
        );
        for down_for in [5, 60, 3600, 86_400 * 30] {
            let delay = probe_delay(Duration::from_secs(down_for));
            assert_eq!(delay, STEADY_PROBE_INTERVAL, "{down_for} s");
        }
        assert!(STEADY_PROBE_INTERVAL >= Duration::from_secs(2));
        assert!(STEADY_PROBE_INTERVAL <= Duration::from_secs(5));
        assert!(FAST_PROBE_WINDOW <= Duration::from_secs(10));
    }

    /// A source down for longer than the fast window is probed at the steady
    /// cadence: one that answers right after a failed probe is seen at the
    /// next steady probe, not within a fast interval, and still seen.
    #[tokio::test]
    async fn a_source_down_past_the_fast_window_is_probed_at_the_steady_cadence() {
        let parked = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = parked.local_addr().unwrap().port();
        drop(parked);
        let source = SourceAddr {
            host: "127.0.0.1".into(),
            port,
        };
        let down_since = tokio::time::Instant::now()
            .checked_sub(FAST_PROBE_WINDOW * 2)
            .unwrap();
        let start = tokio::time::Instant::now();
        let waiter = tokio::spawn({
            let source = source.clone();
            async move { source.wait_reachable(down_since).await }
        });
        let _listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .unwrap();
        tokio::time::sleep(PROBE_INTERVAL * 4).await;
        assert!(!waiter.is_finished(), "no fast probe in steady failure");
        tokio::time::timeout(STEADY_PROBE_INTERVAL * 2, waiter)
            .await
            .expect("the steady probe still sees the source")
            .unwrap();
        let seen = start.elapsed();
        assert!(seen >= STEADY_PROBE_INTERVAL, "{seen:?}");
        assert!(
            seen < STEADY_PROBE_INTERVAL + PROBE_INTERVAL * 4,
            "{seen:?}"
        );
    }

    /// A hostname is resolved outside the connect bound and probed on every
    /// address it resolves to.
    #[tokio::test]
    async fn a_hostname_source_is_resolved_then_probed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source = SourceAddr {
            host: "localhost".into(),
            port: listener.local_addr().unwrap().port(),
        };
        assert!(source.reachable().await);
    }
}
