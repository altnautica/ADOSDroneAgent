//! Host-to-plugin tool invocation registry.
//!
//! The plugin bridge has two flows built into the per-connection loop: the
//! plugin sends the host a request and awaits a response, and the host pushes
//! events/frames to the plugin (no reply). An MCP `tools/call` needs a third:
//! the host asks the plugin to run a tool and awaits the plugin's result. That
//! request originates OUTSIDE the plugin's connection task (a control-socket
//! command from `ados-control`), so it needs a way to reach the live connection.
//!
//! [`InvokeRegistry`] is that seam, decoupled from the (all-sync) [`HostServices`]
//! trait. Each live plugin connection registers an outbound [`InvokeRequest`]
//! sender under its plugin id; [`InvokeRegistry::invoke`] looks the sender up,
//! hands the connection a request plus a one-shot reply channel, and awaits the
//! reply. The connection task writes the `tool.invoke` envelope, tracks the
//! pending reply by request id, and resolves the one-shot when the plugin's
//! `response` frame arrives. A plugin with no live connection, a closed channel,
//! or a slow plugin surfaces as a typed error, never a hang.
//!
//! [`HostServices`]: crate::host::HostServices

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use rmpv::Value;
use tokio::sync::{mpsc, oneshot};

/// The default per-invoke timeout. A tool that needs longer returns a job
/// handle rather than blocking the channel (mirrors the Python host's 5 s).
pub const DEFAULT_INVOKE_TIMEOUT: Duration = Duration::from_secs(5);

/// One outbound tool invocation handed to a live plugin connection. The
/// connection writes it as a `tool.invoke` request and resolves `reply` when the
/// plugin's correlated `response` frame arrives.
#[derive(Debug)]
pub struct InvokeRequest {
    /// The correlation id the connection stamps on the request envelope and
    /// keys its pending map by. Minted by the registry so it is unique.
    pub request_id: String,
    /// The declared tool name to run.
    pub tool: String,
    /// The tool's argument value (an msgpack map, usually).
    pub arguments: Value,
    /// The one-shot the connection resolves with the tool result (or an error
    /// string) when the plugin replies.
    pub reply: oneshot::Sender<Result<Value, String>>,
}

/// The shared per-plugin invoke-sender registry. Held by the server (each
/// connection registers into it) and by the control-socket handler (which calls
/// [`invoke`](Self::invoke)). Cheap to clone behind an `Arc`.
#[derive(Debug, Default)]
pub struct InvokeRegistry {
    senders: Mutex<HashMap<String, mpsc::Sender<InvokeRequest>>>,
    seq: AtomicU64,
}

impl InvokeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) the outbound-request sender for a live plugin
    /// connection. Called when a connection's session is up.
    pub fn register(&self, plugin_id: &str, sender: mpsc::Sender<InvokeRequest>) {
        self.senders
            .lock()
            .expect("invoke registry mutex")
            .insert(plugin_id.to_string(), sender);
    }

    /// Remove a plugin's sender (its connection ended). A later invoke against
    /// it fails closed with `plugin_not_running`.
    pub fn unregister(&self, plugin_id: &str) {
        self.senders
            .lock()
            .expect("invoke registry mutex")
            .remove(plugin_id);
    }

    /// Identity-checked unregister: remove the plugin's sender ONLY when the
    /// stored sender is `sender`'s own channel. On a reconnect overlap the old
    /// connection's teardown must not evict the NEW connection's freshly
    /// registered sender (which would leave the live plugin reporting
    /// `plugin_not_running` until the daemon restarts). `same_channel` compares
    /// channel identity, so an unregister from a superseded connection is a no-op.
    pub fn unregister_if(&self, plugin_id: &str, sender: &mpsc::Sender<InvokeRequest>) {
        let mut map = self.senders.lock().expect("invoke registry mutex");
        if map.get(plugin_id).is_some_and(|s| s.same_channel(sender)) {
            map.remove(plugin_id);
        }
    }

    /// Ask the live plugin connection to run `tool` with `arguments` and return
    /// its result. Fails (never hangs) when the plugin is not connected, its
    /// channel is closed, the connection drops the reply, or the timeout fires.
    pub async fn invoke(
        &self,
        plugin_id: &str,
        tool: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let sender = {
            let map = self.senders.lock().expect("invoke registry mutex");
            map.get(plugin_id).cloned()
        };
        let Some(sender) = sender else {
            return Err(format!("plugin_not_running: {plugin_id}"));
        };
        let request_id = format!("inv-{}", self.seq.fetch_add(1, Ordering::Relaxed));
        let (reply_tx, reply_rx) = oneshot::channel();
        let req = InvokeRequest {
            request_id,
            tool: tool.to_string(),
            arguments,
            reply: reply_tx,
        };
        // One deadline bounds BOTH the enqueue and the reply, so a wedged plugin
        // that stops draining its bounded channel cannot make the enqueue hang
        // (the original `sender.send().await` blocked forever on a full channel,
        // before the reply timeout could ever apply).
        let deadline = tokio::time::Instant::now() + timeout;
        match tokio::time::timeout_at(deadline, sender.send(req)).await {
            Ok(Ok(())) => {}
            // The connection task dropped its receiver between the lookup and
            // the send: treat as not running.
            Ok(Err(_)) => return Err(format!("plugin_not_running: {plugin_id}")),
            // The channel is full and the connection is not draining it.
            Err(_) => return Err(format!("plugin_busy: {plugin_id}")),
        }
        match tokio::time::timeout_at(deadline, reply_rx).await {
            Ok(Ok(result)) => result,
            // The connection dropped the reply sender without answering (it
            // ended mid-invoke).
            Ok(Err(_)) => Err(format!("plugin_disconnected: {plugin_id}")),
            Err(_) => Err(format!("tool_timeout: {tool}")),
        }
    }

    /// Whether a plugin currently has a live connection registered.
    pub fn is_connected(&self, plugin_id: &str) -> bool {
        self.senders
            .lock()
            .expect("invoke registry mutex")
            .contains_key(plugin_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn invoke_with_no_connection_fails_not_running() {
        let reg = InvokeRegistry::new();
        let err = reg
            .invoke("com.x.p", "t", Value::Nil, DEFAULT_INVOKE_TIMEOUT)
            .await
            .unwrap_err();
        assert!(err.contains("plugin_not_running"), "{err}");
    }

    #[tokio::test]
    async fn invoke_round_trips_through_a_registered_sender() {
        let reg = InvokeRegistry::new();
        let (tx, mut rx) = mpsc::channel::<InvokeRequest>(4);
        reg.register("com.x.p", tx);
        // A fake connection task: read the request, reply with the tool name.
        let responder = tokio::spawn(async move {
            let req = rx.recv().await.expect("a request");
            assert_eq!(req.tool, "greet");
            let _ = req.reply.send(Ok(Value::from(format!("ran:{}", req.tool))));
        });
        let out = reg
            .invoke("com.x.p", "greet", Value::Nil, DEFAULT_INVOKE_TIMEOUT)
            .await
            .expect("a result");
        assert_eq!(out, Value::from("ran:greet"));
        responder.await.unwrap();
    }

    #[tokio::test]
    async fn invoke_surfaces_a_dropped_reply_as_disconnected() {
        let reg = InvokeRegistry::new();
        let (tx, mut rx) = mpsc::channel::<InvokeRequest>(4);
        reg.register("com.x.p", tx);
        let dropper = tokio::spawn(async move {
            let req = rx.recv().await.expect("a request");
            drop(req.reply); // end mid-invoke without answering
        });
        let err = reg
            .invoke("com.x.p", "t", Value::Nil, DEFAULT_INVOKE_TIMEOUT)
            .await
            .unwrap_err();
        assert!(err.contains("plugin_disconnected"), "{err}");
        dropper.await.unwrap();
    }

    #[tokio::test]
    async fn invoke_times_out_when_the_connection_never_replies() {
        let reg = InvokeRegistry::new();
        let (tx, mut rx) = mpsc::channel::<InvokeRequest>(4);
        reg.register("com.x.p", tx);
        // Hold the request (and its reply channel) without answering.
        let holder = tokio::spawn(async move {
            let req = rx.recv().await.expect("a request");
            tokio::time::sleep(Duration::from_secs(60)).await;
            let _ = req.reply.send(Ok(Value::Nil));
        });
        let err = reg
            .invoke("com.x.p", "slow", Value::Nil, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(err.contains("tool_timeout"), "{err}");
        holder.abort();
    }

    #[tokio::test]
    async fn unregister_makes_a_plugin_not_running() {
        let reg = InvokeRegistry::new();
        let (tx, _rx) = mpsc::channel::<InvokeRequest>(4);
        reg.register("com.x.p", tx);
        assert!(reg.is_connected("com.x.p"));
        reg.unregister("com.x.p");
        assert!(!reg.is_connected("com.x.p"));
    }

    #[tokio::test]
    async fn invoke_returns_busy_when_the_connection_never_drains() {
        // A capacity-1 channel that is never received from: the first invoke fills
        // the slot and times out (no reply), leaving its request in the buffer; the
        // second invoke cannot enqueue and must return busy rather than hang.
        let reg = InvokeRegistry::new();
        let (tx, _rx) = mpsc::channel::<InvokeRequest>(1);
        reg.register("com.x.p", tx);
        let e1 = reg
            .invoke("com.x.p", "a", Value::Nil, Duration::from_millis(30))
            .await
            .unwrap_err();
        assert!(e1.contains("tool_timeout"), "{e1}");
        let e2 = reg
            .invoke("com.x.p", "b", Value::Nil, Duration::from_millis(30))
            .await
            .unwrap_err();
        assert!(e2.contains("plugin_busy"), "{e2}");
    }

    #[tokio::test]
    async fn unregister_if_only_removes_the_matching_connection() {
        // A reconnect overlap: the OLD connection's teardown must not evict the
        // NEW connection's sender registered under the same plugin id.
        let reg = InvokeRegistry::new();
        let (old_tx, _old_rx) = mpsc::channel::<InvokeRequest>(4);
        reg.register("com.x.p", old_tx.clone());
        // The new connection replaces the sender.
        let (new_tx, _new_rx) = mpsc::channel::<InvokeRequest>(4);
        reg.register("com.x.p", new_tx.clone());
        // The old connection tears down with an identity-checked unregister: it is
        // a no-op because the stored sender is the new one.
        reg.unregister_if("com.x.p", &old_tx);
        assert!(reg.is_connected("com.x.p"), "the new connection survives");
        // The new connection's own identity-checked unregister does remove it.
        reg.unregister_if("com.x.p", &new_tx);
        assert!(!reg.is_connected("com.x.p"));
    }
}
