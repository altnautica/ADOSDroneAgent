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
//! sender under its plugin id; [`InvokeRegistry::invoke`] hands a connection a
//! request plus a one-shot reply channel and awaits the reply. The connection
//! task writes the `tool.invoke` envelope, tracks the pending reply by request
//! id, and resolves the one-shot when the plugin's `response` frame arrives. A
//! plugin with no live connection, a closed channel, or a slow plugin surfaces
//! as a typed error, never a hang.
//!
//! One plugin may hold several sessions at once: its main process, each
//! declared service (`agent.contributes.services` connect under the plugin's
//! id), and a reconnect overlapping its predecessor. Only the process that
//! registered a tool handler can answer a tool, so an invoke is offered to the
//! sessions newest first and passes over each that answers
//! [`TOOL_HANDLER_MISSING`] (nothing ran there) until one answers.
//!
//! [`HostServices`]: crate::host::HostServices

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use ados_protocol::plugin::TOOL_HANDLER_MISSING;
use rmpv::Value;
use tokio::sync::{mpsc, oneshot};

/// The default per-invoke timeout. A tool that needs longer returns a job
/// handle rather than blocking the channel.
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
    /// Each plugin's live sessions in registration order, newest last.
    senders: Mutex<HashMap<String, Vec<mpsc::Sender<InvokeRequest>>>>,
    seq: AtomicU64,
}

impl InvokeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the outbound-request sender of a live plugin connection,
    /// beside any other live session of the same plugin. Called when a
    /// connection's session is up.
    pub fn register(&self, plugin_id: &str, sender: mpsc::Sender<InvokeRequest>) {
        self.senders
            .lock()
            .expect("invoke registry mutex")
            .entry(plugin_id.to_string())
            .or_default()
            .push(sender);
    }

    /// Identity-checked unregister: remove only `sender`'s own channel, so an
    /// ended session never evicts another live session of the same plugin (a
    /// declared service, or the successor of a reconnect overlap). With no
    /// session left, a later invoke fails closed with `plugin_not_running`.
    pub fn unregister_if(&self, plugin_id: &str, sender: &mpsc::Sender<InvokeRequest>) {
        let mut map = self.senders.lock().expect("invoke registry mutex");
        if let Some(senders) = map.get_mut(plugin_id) {
            senders.retain(|held| !held.same_channel(sender));
            if senders.is_empty() {
                map.remove(plugin_id);
            }
        }
    }

    /// Ask the plugin to run `tool` with `arguments` and return its result.
    ///
    /// The plugin's live sessions are asked newest first; a session that
    /// answers [`TOOL_HANDLER_MISSING`] or has already closed its channel is
    /// passed over for the next. Any other answer is final, error or not, so a
    /// tool runs at most once. One `timeout` bounds the whole call. Fails
    /// (never hangs) when no session is connected, a connection drops the
    /// reply, or the timeout fires.
    pub async fn invoke(
        &self,
        plugin_id: &str,
        tool: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let senders = {
            let map = self.senders.lock().expect("invoke registry mutex");
            map.get(plugin_id).cloned().unwrap_or_default()
        };
        // One deadline bounds every enqueue and reply, so a wedged plugin that
        // stops draining its bounded channel cannot make the enqueue hang
        // (the original `sender.send().await` blocked forever on a full channel,
        // before the reply timeout could ever apply).
        let deadline = tokio::time::Instant::now() + timeout;
        let mut arguments = Some(arguments);
        let mut handler_missing = false;
        for (i, sender) in senders.iter().enumerate().rev() {
            // The oldest session is the last one asked: it takes the arguments
            // and every newer one gets a copy.
            let arguments = if i == 0 {
                arguments.take().unwrap_or(Value::Nil)
            } else {
                arguments.clone().unwrap_or(Value::Nil)
            };
            let request_id = format!("inv-{}", self.seq.fetch_add(1, Ordering::Relaxed));
            let (reply_tx, reply_rx) = oneshot::channel();
            let req = InvokeRequest {
                request_id,
                tool: tool.to_string(),
                arguments,
                reply: reply_tx,
            };
            match tokio::time::timeout_at(deadline, sender.send(req)).await {
                Ok(Ok(())) => {}
                // The connection task dropped its receiver after the lookup:
                // that session has ended, so ask the next.
                Ok(Err(_)) => continue,
                // The channel is full and the connection is not draining it.
                Err(_) => return Err(format!("plugin_busy: {plugin_id}")),
            }
            match tokio::time::timeout_at(deadline, reply_rx).await {
                Ok(Ok(Err(e))) if e == TOOL_HANDLER_MISSING => handler_missing = true,
                Ok(Ok(result)) => return result,
                // The connection dropped the reply sender without answering (it
                // ended mid-invoke).
                Ok(Err(_)) => return Err(format!("plugin_disconnected: {plugin_id}")),
                Err(_) => return Err(format!("tool_timeout: {tool}")),
            }
        }
        if handler_missing {
            Err(TOOL_HANDLER_MISSING.to_string())
        } else {
            Err(format!("plugin_not_running: {plugin_id}"))
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
    async fn unregistering_the_last_session_makes_a_plugin_not_running() {
        let reg = InvokeRegistry::new();
        let (tx, _rx) = mpsc::channel::<InvokeRequest>(4);
        reg.register("com.x.p", tx.clone());
        assert!(reg.is_connected("com.x.p"));
        reg.unregister_if("com.x.p", &tx);
        assert!(!reg.is_connected("com.x.p"));
    }

    /// A fake session that answers every request with `answer` and records them.
    fn session(
        reg: &InvokeRegistry,
        answer: Result<&'static str, &'static str>,
    ) -> (
        mpsc::Sender<InvokeRequest>,
        tokio::task::JoinHandle<Vec<(String, Value)>>,
    ) {
        let (tx, mut rx) = mpsc::channel::<InvokeRequest>(4);
        reg.register("com.x.p", tx.clone());
        let task = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(req) = rx.recv().await {
                seen.push((req.tool.clone(), req.arguments.clone()));
                let _ = req
                    .reply
                    .send(answer.map(Value::from).map_err(|e| e.to_string()));
            }
            seen
        });
        (tx, task)
    }

    #[tokio::test]
    async fn a_session_without_a_tool_handler_passes_the_call_to_the_one_with_it() {
        // The plugin's main process serves the tools; its declared service
        // connects later under the same id and serves none.
        let reg = InvokeRegistry::new();
        let (main_tx, main) = session(&reg, Ok("started"));
        let (service_tx, service) = session(&reg, Err(TOOL_HANDLER_MISSING));
        let args = Value::Map(vec![(Value::from("site"), Value::from("bay"))]);
        let out = reg
            .invoke(
                "com.x.p",
                "capture_start",
                args.clone(),
                DEFAULT_INVOKE_TIMEOUT,
            )
            .await
            .expect("the main process answers");
        assert_eq!(out, Value::from("started"));
        reg.unregister_if("com.x.p", &main_tx);
        reg.unregister_if("com.x.p", &service_tx);
        drop((main_tx, service_tx));
        let expected = vec![("capture_start".to_string(), args)];
        assert_eq!(service.await.unwrap(), expected, "asked first, newest");
        assert_eq!(main.await.unwrap(), expected, "then the main process");
    }

    #[tokio::test]
    async fn the_newest_session_with_a_handler_answers_alone() {
        // A reconnect overlap: the new session answers, the old one is never asked.
        let reg = InvokeRegistry::new();
        let (old_tx, old) = session(&reg, Ok("old"));
        let (new_tx, new) = session(&reg, Ok("new"));
        let out = reg
            .invoke("com.x.p", "t", Value::Nil, DEFAULT_INVOKE_TIMEOUT)
            .await
            .expect("a result");
        assert_eq!(out, Value::from("new"));
        reg.unregister_if("com.x.p", &old_tx);
        reg.unregister_if("com.x.p", &new_tx);
        drop((old_tx, new_tx));
        assert!(old.await.unwrap().is_empty());
        assert_eq!(new.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_tool_error_is_final_and_never_offered_to_another_session() {
        let reg = InvokeRegistry::new();
        let (old_tx, old) = session(&reg, Ok("ran twice"));
        let (new_tx, _new) = session(&reg, Err("no storage"));
        let err = reg
            .invoke("com.x.p", "t", Value::Nil, DEFAULT_INVOKE_TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(err, "no storage");
        reg.unregister_if("com.x.p", &old_tx);
        reg.unregister_if("com.x.p", &new_tx);
        drop((old_tx, new_tx));
        assert!(old.await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_session_with_a_handler_answers_tool_handler_missing() {
        let reg = InvokeRegistry::new();
        let (_a, _ta) = session(&reg, Err(TOOL_HANDLER_MISSING));
        let (_b, _tb) = session(&reg, Err(TOOL_HANDLER_MISSING));
        let err = reg
            .invoke("com.x.p", "t", Value::Nil, DEFAULT_INVOKE_TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(err, TOOL_HANDLER_MISSING);
    }

    #[tokio::test]
    async fn a_closed_session_is_passed_over() {
        let reg = InvokeRegistry::new();
        let (live_tx, _live) = session(&reg, Ok("live"));
        // A session whose connection task already ended, not yet unregistered.
        let (dead_tx, dead_rx) = mpsc::channel::<InvokeRequest>(4);
        reg.register("com.x.p", dead_tx);
        drop(dead_rx);
        let out = reg
            .invoke("com.x.p", "t", Value::Nil, DEFAULT_INVOKE_TIMEOUT)
            .await
            .expect("the live session answers");
        assert_eq!(out, Value::from("live"));
        drop(live_tx);
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
        let (new_tx, _new_rx) = mpsc::channel::<InvokeRequest>(4);
        reg.register("com.x.p", new_tx.clone());
        // The old connection tears down with an identity-checked unregister:
        // the new connection's sender stays.
        reg.unregister_if("com.x.p", &old_tx);
        assert!(reg.is_connected("com.x.p"), "the new connection survives");
        // A repeated unregister of the old sender is a no-op.
        reg.unregister_if("com.x.p", &old_tx);
        assert!(reg.is_connected("com.x.p"));
        // The new connection's own identity-checked unregister does remove it.
        reg.unregister_if("com.x.p", &new_tx);
        assert!(!reg.is_connected("com.x.p"));
    }
}
