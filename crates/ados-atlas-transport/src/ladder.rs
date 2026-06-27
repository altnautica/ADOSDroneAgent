//! The bearer failover ladder.
//!
//! Holds the configured bearers ordered by priority (local-first) and sends an
//! event over the first one that is available and accepts it, falling to the
//! next on failure. This is the same topology-driven failover the network uplink
//! matrix uses: a usable local bearer always wins over the cloud lane.

use ados_protocol::atlas::AtlasEvent;

use crate::{AtlasBearer, BearerKind, TransportError};

/// An ordered set of bearers with priority failover.
pub struct BearerLadder {
    bearers: Vec<Box<dyn AtlasBearer>>,
}

impl BearerLadder {
    /// Build a ladder from a set of bearers; they are sorted by priority so the
    /// most-preferred (local-first) bearer is tried first regardless of the order
    /// they are passed in.
    pub fn new(mut bearers: Vec<Box<dyn AtlasBearer>>) -> Self {
        bearers.sort_by_key(|b| b.kind().priority());
        Self { bearers }
    }

    /// The bearer kinds in this ladder, in priority order.
    pub fn kinds(&self) -> Vec<BearerKind> {
        self.bearers.iter().map(|b| b.kind()).collect()
    }

    /// Send an event over the first available bearer that accepts it. Returns the
    /// bearer that carried it. A bearer that reports unavailable is skipped (not a
    /// failure); a bearer that is available but fails with a *retriable* error is
    /// recorded and the ladder falls through to the next. A *non-retriable* error
    /// (a 4xx / an encode fault — the event itself is bad, so every bearer would
    /// reject it) returns immediately rather than burning down to the cloud lane.
    /// Returns [`TransportError::NoBearer`] only when no bearer carried the event
    /// and none produced an error (an empty or all-unavailable ladder).
    pub async fn send(&self, event: &AtlasEvent) -> Result<BearerKind, TransportError> {
        let mut last_err: Option<TransportError> = None;
        for bearer in &self.bearers {
            if !bearer.is_available().await {
                continue;
            }
            match bearer.send(event).await {
                Ok(()) => return Ok(bearer.kind()),
                Err(e) if !e.is_retriable() => return Err(e),
                Err(e) => {
                    tracing::debug!(kind = ?bearer.kind(), error = %e, "atlas bearer send failed, falling over");
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or(TransportError::NoBearer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LoopbackBearer;

    fn event() -> AtlasEvent {
        AtlasEvent {
            topic: "atlas.keyframe".into(),
            payload: vec![9],
        }
    }

    #[tokio::test]
    async fn ladder_sorts_bearers_by_priority() {
        // Pass them out of order; the ladder sorts local-first.
        let (lo, _r1) = LoopbackBearer::channel();
        let (hi, _r2) = LoopbackBearer::channel();
        let ladder = BearerLadder::new(vec![Box::new(hi), Box::new(lo)]);
        assert_eq!(
            ladder.kinds(),
            vec![BearerKind::Loopback, BearerKind::Loopback]
        );
    }

    #[tokio::test]
    async fn send_uses_the_first_available_bearer() {
        let (primary, mut rx_primary) = LoopbackBearer::channel();
        let (secondary, mut rx_secondary) = LoopbackBearer::channel();
        let ladder = BearerLadder::new(vec![Box::new(primary), Box::new(secondary)]);

        let carried = ladder.send(&event()).await.unwrap();
        assert_eq!(carried, BearerKind::Loopback);
        // The first bearer carried it; the second did not.
        assert!(rx_primary.try_recv().is_ok());
        assert!(rx_secondary.try_recv().is_err());
    }

    #[tokio::test]
    async fn send_falls_over_when_the_preferred_bearer_is_unavailable() {
        let (primary, mut rx_primary) = LoopbackBearer::channel();
        primary.set_available(false); // the preferred bearer has no carrier
        let (secondary, mut rx_secondary) = LoopbackBearer::channel();
        let ladder = BearerLadder::new(vec![Box::new(primary), Box::new(secondary)]);

        ladder.send(&event()).await.unwrap();
        // Skipped the unavailable primary, carried on the secondary.
        assert!(rx_primary.try_recv().is_err());
        assert!(rx_secondary.try_recv().is_ok());
    }

    #[tokio::test]
    async fn no_available_bearer_is_an_error() {
        let (primary, _rx) = LoopbackBearer::channel();
        primary.set_available(false);
        let ladder = BearerLadder::new(vec![Box::new(primary)]);
        assert!(matches!(
            ladder.send(&event()).await,
            Err(TransportError::NoBearer)
        ));
    }

    /// A bearer that is always available but fails every send (a reachable peer
    /// that errors). Sorts as `Loopback` so it can be ordered ahead of a real
    /// LoopbackBearer in a test (stable sort keeps insertion order on a tie).
    struct FailingBearer {
        retriable: bool,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl AtlasBearer for FailingBearer {
        fn kind(&self) -> BearerKind {
            BearerKind::Loopback
        }
        async fn is_available(&self) -> bool {
            true
        }
        async fn send(&self, _event: &AtlasEvent) -> Result<(), TransportError> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Err(if self.retriable {
                TransportError::Request("boom".into())
            } else {
                TransportError::Http(400)
            })
        }
    }

    #[tokio::test]
    async fn an_available_bearer_that_errors_falls_over_to_the_next() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let failing = FailingBearer {
            retriable: true,
            calls: calls.clone(),
        };
        let (good, mut rx) = LoopbackBearer::channel();
        // Both sort as Loopback; the stable sort keeps [failing, good] order.
        let ladder = BearerLadder::new(vec![Box::new(failing), Box::new(good)]);
        ladder.send(&event()).await.unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(rx.try_recv().is_ok()); // the good bearer carried it
    }

    #[tokio::test]
    async fn all_bearers_erroring_returns_the_last_error_not_no_bearer() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ladder = BearerLadder::new(vec![
            Box::new(FailingBearer {
                retriable: true,
                calls: calls.clone(),
            }),
            Box::new(FailingBearer {
                retriable: true,
                calls: calls.clone(),
            }),
        ]);
        let err = ladder.send(&event()).await.unwrap_err();
        assert!(matches!(err, TransportError::Request(_)));
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn a_non_retriable_error_returns_immediately_without_falling_over() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let failing = FailingBearer {
            retriable: false, // a 4xx: the event is bad
            calls: calls.clone(),
        };
        let (good, mut rx) = LoopbackBearer::channel();
        let ladder = BearerLadder::new(vec![Box::new(failing), Box::new(good)]);
        let err = ladder.send(&event()).await.unwrap_err();
        assert!(matches!(err, TransportError::Http(400)));
        // The lower-priority bearer was NEVER tried.
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(rx.try_recv().is_err());
    }
}
