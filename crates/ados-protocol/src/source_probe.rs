//! Reachability probe for a network camera source.
//!
//! A process that opens an RTSP or HTTP camera while the camera is unreachable
//! gets stuck in the kernel's connect retry: a dropped SYN is retransmitted
//! after 1 s, 3 s, 7 s, 15 s and so on, so a camera that comes back in the
//! middle of that schedule is only seen at the next retransmit, seconds late.
//! A daemon that has to reopen such a source probes it instead: one short TCP
//! connect every [`PROBE_INTERVAL`], each bounded by the same interval, and it
//! opens the source only once a probe is accepted. A camera that comes back is
//! then seen within one interval of answering, and the cost while it is down is
//! one SYN per interval.

use std::time::Duration;

use tokio::net::TcpStream;

/// The probe cadence, and the bound on each probe's connect.
pub const PROBE_INTERVAL: Duration = Duration::from_millis(250);

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

    /// One probe: does the source accept a TCP connection within
    /// [`PROBE_INTERVAL`]? The connection is closed at once.
    pub async fn reachable(&self) -> bool {
        matches!(
            tokio::time::timeout(
                PROBE_INTERVAL,
                TcpStream::connect((self.host.as_str(), self.port))
            )
            .await,
            Ok(Ok(_))
        )
    }

    /// Resolve once the source accepts a connection, probing every
    /// [`PROBE_INTERVAL`] (the first probe at once). Never gives up; drop the
    /// future to stop probing.
    pub async fn wait_reachable(&self) {
        let mut tick = tokio::time::interval(PROBE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
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
            SourceAddr::from_url("rtsp://192.168.150.1:9754/cam"),
            addr("192.168.150.1", 9754)
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
            async move { source.wait_reachable().await }
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
}
