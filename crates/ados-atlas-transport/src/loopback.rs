//! The in-process bearer: an unbounded channel. This is the test + co-located-dev
//! bearer (a drone, compute, and GCS on one box for development); the production
//! carrier is the LAN-HTTP bearer, whose receiver is bounded for backpressure.
//! The channel is intentionally unbounded here so a test never blocks; do not use
//! it as a production hot path. Its availability is toggleable so a test can model
//! a bearer dropping and the ladder failing over.

use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use ados_protocol::atlas::AtlasEvent;

use crate::{AtlasBearer, BearerKind, TransportError};

/// An in-process bearer over an unbounded channel.
pub struct LoopbackBearer {
    tx: UnboundedSender<AtlasEvent>,
    available: AtomicBool,
}

impl LoopbackBearer {
    /// Build a bearer and its receiving end. Events sent over the bearer arrive
    /// on the returned receiver.
    pub fn channel() -> (Self, UnboundedReceiver<AtlasEvent>) {
        let (tx, rx) = unbounded_channel();
        (
            Self {
                tx,
                available: AtomicBool::new(true),
            },
            rx,
        )
    }

    /// Mark the bearer available or not (model a carrier dropping).
    pub fn set_available(&self, available: bool) {
        self.available.store(available, Ordering::Relaxed);
    }
}

#[async_trait::async_trait]
impl AtlasBearer for LoopbackBearer {
    fn kind(&self) -> BearerKind {
        BearerKind::Loopback
    }

    async fn is_available(&self) -> bool {
        self.available.load(Ordering::Relaxed) && !self.tx.is_closed()
    }

    async fn send(&self, event: &AtlasEvent) -> Result<(), TransportError> {
        if !self.available.load(Ordering::Relaxed) {
            return Err(TransportError::Unavailable);
        }
        self.tx
            .send(event.clone())
            .map_err(|_| TransportError::Closed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(topic: &str) -> AtlasEvent {
        AtlasEvent::new(topic, None, vec![1, 2, 3])
    }

    #[tokio::test]
    async fn a_sent_event_arrives_on_the_receiver() {
        let (bearer, mut rx) = LoopbackBearer::channel();
        assert!(bearer.is_available().await);
        bearer.send(&event("atlas.keyframe")).await.unwrap();
        let got = rx.recv().await.unwrap();
        assert_eq!(got.topic, "atlas.keyframe");
        assert_eq!(got.payload, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn an_unavailable_bearer_refuses_to_send() {
        let (bearer, _rx) = LoopbackBearer::channel();
        bearer.set_available(false);
        assert!(!bearer.is_available().await);
        assert!(matches!(
            bearer.send(&event("x")).await,
            Err(TransportError::Unavailable)
        ));
    }

    #[tokio::test]
    async fn a_closed_receiver_makes_send_fail() {
        let (bearer, rx) = LoopbackBearer::channel();
        drop(rx);
        assert!(!bearer.is_available().await);
        assert!(matches!(
            bearer.send(&event("x")).await,
            Err(TransportError::Closed)
        ));
    }
}
