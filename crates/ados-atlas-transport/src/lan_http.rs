//! The direct LAN/WiFi HTTP bearer — the first-class production path.
//!
//! The sender ([`LanHttpBearer`]) POSTs a msgpack-framed [`AtlasEvent`] to the
//! compute node's atlas-event endpoint; the receiver ([`atlas_event_router`]) is
//! the axum router the compute node mounts to decode events onto a bounded
//! channel its ingest loop drains. Plain HTTP on the LAN (no TLS); reach is
//! local-first (mDNS + LAN-pair). The sender carries explicit connect + request
//! timeouts so a hung-but-reachable node fails the send (and the ladder fails
//! over) instead of parking forever.

use std::time::Duration;

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    routing::{get, post},
    Router,
};
use reqwest::Client;
use tokio::sync::mpsc::{error::TrySendError, Sender};

use ados_protocol::atlas::AtlasEvent;

use crate::{AtlasBearer, BearerKind, TransportError};

/// The path the compute node receives Atlas events on.
const EVENT_PATH: &str = "/api/atlas/event";
/// A cheap liveness path the sender can probe.
const HEALTH_PATH: &str = "/api/atlas/health";
/// The largest event body the receiver accepts. A `Full`-tier keyframe is the
/// full-resolution image bytes plus the IMU window (a few MB per camera), so the
/// default 2 MB axum limit would reject the documented payload; bulk bags ride a
/// separate resumable lane, so this caps a single event, not a whole bag.
const MAX_EVENT_BYTES: usize = 64 * 1024 * 1024;
/// Connect + total-request timeouts for the sender, so a half-open or hung peer
/// fails the send rather than wedging the ladder.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// A shorter ceiling for the explicit reachability probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// A bearer that streams events to a compute node over LAN HTTP.
pub struct LanHttpBearer {
    client: Client,
    base_url: String,
}

impl LanHttpBearer {
    /// A bearer targeting `base_url` (e.g. `http://compute.local:8092`), with a
    /// client that has connect + request timeouts so a hung peer fails the send.
    pub fn new(base_url: impl Into<String>) -> Self {
        // Install the process-default crypto provider first, or build() can
        // panic "No provider set" under the workspace's no-provider rustls path.
        ados_protocol::crypto::ensure_crypto_provider();
        // build() only fails on a TLS-stack init error; `expect` keeps the
        // timeout guarantee rather than silently degrading to a timeout-less
        // default (which would let a hung peer park the send).
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("build atlas lan-http client");
        Self::with_client(client, base_url)
    }

    /// A bearer reusing an existing client (share one pool + timeout config
    /// across bearers). The caller is responsible for setting timeouts.
    pub fn with_client(client: Client, base_url: impl Into<String>) -> Self {
        let base = base_url.into().trim_end_matches('/').to_string();
        Self {
            client,
            base_url: base,
        }
    }

    /// An explicit reachability probe (a cheap, short-timeout GET to the health
    /// path). Distinct from [`AtlasBearer::is_available`], which stays optimistic
    /// so the ladder does not pay a network round-trip before every send — a send
    /// failure (now bounded by the request timeout) is the failover signal.
    pub async fn probe(&self) -> bool {
        self.client
            .get(format!("{}{HEALTH_PATH}", self.base_url))
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }
}

#[async_trait::async_trait]
impl AtlasBearer for LanHttpBearer {
    fn kind(&self) -> BearerKind {
        BearerKind::DirectLan
    }

    async fn is_available(&self) -> bool {
        // Optimistic: the ladder tries this bearer and falls over on a send
        // failure (bounded by the request timeout) rather than paying a health
        // round-trip per event. Use `probe()` for an explicit reachability check.
        true
    }

    async fn send(&self, event: &AtlasEvent) -> Result<(), TransportError> {
        let body = event.to_msgpack()?;
        let resp = self
            .client
            .post(format!("{}{EVENT_PATH}", self.base_url))
            .header("content-type", "application/msgpack")
            .body(body)
            .send()
            .await
            .map_err(|e| TransportError::Request(e.to_string()))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(TransportError::Http(resp.status().as_u16()))
        }
    }
}

/// The axum router the compute node mounts to receive Atlas events. A decoded
/// event is forwarded on the bounded `sink`; the ingest loop drains the receiver.
/// A malformed body is a `400`, a full or gone ingest channel a `503`
/// (backpressure — the sender's ladder retries or drops), an over-limit body a
/// `413`, never a panic or an unbounded queue.
pub fn atlas_event_router(sink: Sender<AtlasEvent>) -> Router {
    Router::new()
        .route(EVENT_PATH, post(receive_event))
        .route(HEALTH_PATH, get(health))
        .layer(DefaultBodyLimit::max(MAX_EVENT_BYTES))
        .with_state(sink)
}

async fn health() -> StatusCode {
    StatusCode::OK
}

async fn receive_event(State(sink): State<Sender<AtlasEvent>>, body: Bytes) -> StatusCode {
    match AtlasEvent::from_msgpack(&body) {
        Ok(event) => match sink.try_send(event) {
            Ok(()) => StatusCode::ACCEPTED,
            // Full = the reconstructor is behind (backpressure); Closed = the
            // ingest loop is gone. Both tell the sender to back off, not retry
            // blindly.
            Err(TrySendError::Full(_)) | Err(TrySendError::Closed(_)) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
        },
        Err(_) => StatusCode::BAD_REQUEST,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::{channel, Receiver};

    fn keyframe_event() -> AtlasEvent {
        AtlasEvent {
            topic: "atlas.keyframe".into(),
            payload: vec![1, 2, 3, 4],
        }
    }

    async fn spawn_server() -> (std::net::SocketAddr, Receiver<AtlasEvent>) {
        let (sink, rx) = channel(64);
        let app = atlas_event_router(sink);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (addr, rx)
    }

    #[tokio::test]
    async fn a_sent_event_is_received_over_lan_http() {
        let (addr, mut rx) = spawn_server().await;
        let bearer = LanHttpBearer::new(format!("http://{addr}"));
        assert!(bearer.probe().await);
        bearer.send(&keyframe_event()).await.unwrap();
        let got = rx.recv().await.unwrap();
        assert_eq!(got.topic, "atlas.keyframe");
        assert_eq!(got.payload, vec![1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn a_multi_megabyte_keyframe_is_accepted_not_413() {
        // A Full-tier keyframe (full-resolution image bytes) exceeds axum's
        // default 2 MB limit; the raised limit must accept it on the primary path.
        let (addr, mut rx) = spawn_server().await;
        let bearer = LanHttpBearer::new(format!("http://{addr}"));
        let big = AtlasEvent {
            topic: "atlas.keyframe".into(),
            payload: vec![0xAB; 5 * 1024 * 1024], // 5 MB
        };
        bearer.send(&big).await.unwrap(); // Ok(()) means a 2xx, not a 413
        let got = rx.recv().await.unwrap();
        assert_eq!(got.payload.len(), 5 * 1024 * 1024);
    }

    #[tokio::test]
    async fn probe_is_false_when_nothing_is_listening() {
        // Port 1 is privileged and unbound; the connect fails fast.
        let bearer = LanHttpBearer::new("http://127.0.0.1:1");
        assert!(!bearer.probe().await);
        // is_available stays optimistic; send is what fails + triggers failover.
        assert!(bearer.is_available().await);
        assert!(bearer.send(&keyframe_event()).await.is_err());
    }

    #[tokio::test]
    async fn a_non_success_status_maps_to_transport_error_http() {
        // A server that always 500s; the bearer surfaces Http(500) so the ladder
        // can decide (a 5xx is retriable).
        let app = Router::new().route(
            EVENT_PATH,
            post(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let bearer = LanHttpBearer::new(format!("http://{addr}"));
        match bearer.send(&keyframe_event()).await {
            Err(TransportError::Http(500)) => {}
            other => panic!("expected Http(500), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_malformed_body_is_a_bad_request_not_a_panic() {
        let (sink, _rx) = channel(4);
        let status = receive_event(State(sink), Bytes::from_static(b"not-msgpack")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_gone_ingest_channel_is_service_unavailable() {
        let (sink, rx) = channel(4);
        drop(rx); // the ingest loop is gone
        let body = Bytes::from(keyframe_event().to_msgpack().unwrap());
        let status = receive_event(State(sink), body).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn a_full_ingest_channel_backpressures_with_503() {
        let (sink, _rx) = channel(1);
        // Fill the one slot (never drained), then the next event is refused.
        let body = Bytes::from(keyframe_event().to_msgpack().unwrap());
        assert_eq!(
            receive_event(State(sink.clone()), body.clone()).await,
            StatusCode::ACCEPTED
        );
        assert_eq!(
            receive_event(State(sink), body).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
