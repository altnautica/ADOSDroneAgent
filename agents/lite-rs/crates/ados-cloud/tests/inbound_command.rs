//! Integration tests for the inbound `command` topic dispatcher.
//!
//! Covers the three behaviours the operator depends on:
//!   1. `reboot` is gated on `cloud.allow_reboot`. Disabled drops the
//!      command; enabled triggers the (mock) reboot provider.
//!   2. Duplicate `request_id` deliveries are dropped — QoS 1 retries
//!      and broker reconnect fan-out must not double-execute.
//!   3. `status_request` fires an immediate heartbeat on the trigger
//!      channel.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use ados_cloud::handlers::{
    CommandHandler, CommandOutcome, RebootProvider, REBOOT_GRACE_SECS,
};

/// Captures schedule_reboot calls so the test can assert the count
/// AND the grace period the dispatcher passed through.
struct MockRebootProvider {
    calls: Arc<AtomicU64>,
    last_grace_secs: Arc<AtomicU64>,
}

impl MockRebootProvider {
    fn new() -> (Arc<Self>, Arc<AtomicU64>, Arc<AtomicU64>) {
        let calls = Arc::new(AtomicU64::new(0));
        let last_grace = Arc::new(AtomicU64::new(0));
        let provider = Arc::new(Self {
            calls: calls.clone(),
            last_grace_secs: last_grace.clone(),
        });
        (provider, calls, last_grace)
    }
}

impl RebootProvider for MockRebootProvider {
    fn schedule_reboot(&self, grace_secs: u64) -> std::io::Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.last_grace_secs.store(grace_secs, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn reboot_disabled_drops_command() {
    let (provider, calls, _) = MockRebootProvider::new();
    let (trigger_tx, _trigger_rx) = tokio::sync::mpsc::channel(8);
    let handler = CommandHandler::new(Some(trigger_tx), provider, /* allow_reboot */ false);

    let payload = serde_json::to_vec(&serde_json::json!({
        "request_id": "req-disabled-1",
        "type": "reboot",
    }))
    .unwrap();

    let outcome = handler.dispatch(&payload).await;

    assert!(
        matches!(outcome, CommandOutcome::Disabled { ref command_type } if command_type == "reboot"),
        "expected Disabled outcome, got {:?}",
        outcome
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "reboot provider must NOT be called when allow_reboot is false"
    );
}

#[tokio::test]
async fn reboot_enabled_invokes_provider_with_grace() {
    let (provider, calls, last_grace) = MockRebootProvider::new();
    let (trigger_tx, _trigger_rx) = tokio::sync::mpsc::channel(8);
    let handler = CommandHandler::new(Some(trigger_tx), provider, /* allow_reboot */ true);

    let payload = serde_json::to_vec(&serde_json::json!({
        "request_id": "req-enabled-1",
        "type": "reboot",
    }))
    .unwrap();

    let outcome = handler.dispatch(&payload).await;

    assert!(
        matches!(outcome, CommandOutcome::Executed { ref command_type } if command_type == "reboot"),
        "expected Executed outcome, got {:?}",
        outcome
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "reboot provider must be invoked exactly once when allow_reboot is true"
    );
    assert_eq!(
        last_grace.load(Ordering::SeqCst),
        REBOOT_GRACE_SECS,
        "dispatcher must pass through the documented grace period"
    );
}

#[tokio::test]
async fn duplicate_request_id_runs_handler_once() {
    let (provider, calls, _) = MockRebootProvider::new();
    let (trigger_tx, _trigger_rx) = tokio::sync::mpsc::channel(8);
    let handler = CommandHandler::new(Some(trigger_tx), provider, /* allow_reboot */ true);

    let payload = serde_json::to_vec(&serde_json::json!({
        "request_id": "req-dup-1",
        "type": "reboot",
    }))
    .unwrap();

    // First delivery executes.
    let outcome1 = handler.dispatch(&payload).await;
    assert!(matches!(outcome1, CommandOutcome::Executed { .. }));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Second delivery with the same request_id is a no-op.
    let outcome2 = handler.dispatch(&payload).await;
    assert!(
        matches!(outcome2, CommandOutcome::DuplicateDropped { ref request_id } if request_id == "req-dup-1"),
        "expected DuplicateDropped, got {:?}",
        outcome2
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "duplicate delivery must NOT re-invoke the reboot provider"
    );

    // Third delivery same payload — still dropped.
    let outcome3 = handler.dispatch(&payload).await;
    assert!(matches!(outcome3, CommandOutcome::DuplicateDropped { .. }));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn status_request_fires_heartbeat_trigger() {
    let (provider, _, _) = MockRebootProvider::new();
    let (trigger_tx, mut trigger_rx) = tokio::sync::mpsc::channel(4);
    let handler = CommandHandler::new(Some(trigger_tx), provider, /* allow_reboot */ false);

    let payload = serde_json::to_vec(&serde_json::json!({
        "request_id": "req-status-1",
        "type": "status_request",
    }))
    .unwrap();

    let outcome = handler.dispatch(&payload).await;

    assert!(
        matches!(outcome, CommandOutcome::Executed { ref command_type } if command_type == "status_request"),
        "expected Executed outcome, got {:?}",
        outcome
    );

    let recv = tokio::time::timeout(std::time::Duration::from_millis(100), trigger_rx.recv()).await;
    assert!(
        matches!(recv, Ok(Some(()))),
        "heartbeat trigger channel must receive an event after status_request"
    );
}

#[tokio::test]
async fn status_request_without_trigger_is_a_noop() {
    // Lite agent built without a heartbeat trigger wired (e.g. an
    // offline-bootstrap path that has no cloud heartbeat task).
    // status_request must not error; it just logs and drops.
    let (provider, _, _) = MockRebootProvider::new();
    let handler = CommandHandler::new(None, provider, /* allow_reboot */ false);

    let payload = serde_json::to_vec(&serde_json::json!({
        "request_id": "req-status-no-trigger",
        "type": "status_request",
    }))
    .unwrap();

    let outcome = handler.dispatch(&payload).await;
    assert!(matches!(outcome, CommandOutcome::Executed { .. }));
}
