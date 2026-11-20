//! Cloud command-poll loop.
//!
//! Every 5 s, when paired, GET `{convex}/agent/commands?deviceId=...` with
//! `X-ADOS-Key`, dispatch each command, and ACK the result to
//! `{convex}/agent/commands/ack`. The auth is always the `X-ADOS-Key` header,
//! never a URL/query param.
//!
//! One pass ([`poll_once`]) runs against two seams: the cloud queue
//! ([`CommandLink`]: the claim and one ack POST) and the local handlers
//! ([`CommandRunner`]). The daemon backs them with HTTPS and the real
//! dispatcher; the in-process simulation (`tests/dst`) backs them with a
//! simulated queue and node.

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

use crate::dispatch::executed_commands::{ExecutedCommands, Precheck, INTERRUPTED};
use crate::dispatch::{CommandResult, CommandStatus};

/// Poll cadence: a 5 s sleep, as the receiver expects.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Delays before each ack attempt: one immediate try, then two retries.
pub const ACK_RETRY_DELAYS: [Duration; 3] = [
    Duration::ZERO,
    Duration::from_millis(500),
    Duration::from_secs(2),
];

/// What one ack POST got back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckAttempt {
    /// A 2xx answer: the queue has the ack.
    Delivered,
    /// A non-2xx answer, with its HTTP status.
    Refused(u16),
    /// No answer (a transport error).
    Unanswered,
}

/// The cloud command queue, as one poll pass sees it.
pub trait CommandLink {
    /// Claim the queue (`GET /agent/commands`): the response body, or `None`
    /// on a transport error, a non-2xx answer or an unreadable body.
    fn claim(&self) -> impl Future<Output = Option<serde_json::Value>>;

    /// POST one ack (`/agent/commands/ack`), once.
    fn ack(&self, ack: &serde_json::Value) -> impl Future<Output = AckAttempt>;
}

/// The node's command handlers.
pub trait CommandRunner {
    /// Run one command to its result.
    fn run(&self, name: &str, cmd: &serde_json::Value) -> impl Future<Output = CommandResult>;
}

/// How long acks the queue cannot be reached for may hold the claim back. By
/// then the queue's stuck-command sweep has failed every row they answer, and
/// it ignores a late ack, so holding them longer would only keep new commands
/// out.
pub const ACK_OUTBOX_MAX_HOLD: Duration = Duration::from_secs(60 * 60);

/// Delivery passes the queue may answer an ack with a retryable error (5xx,
/// 408, 429) before the node gives that ack up. A refused ack holds the claim
/// back only until then, so one ack the queue keeps failing on cannot keep
/// other commands out for more than about half a minute.
pub const ACK_MAX_REFUSALS: u32 = 5;

/// The queue's delivery window for a row queued with no TTL; a row carries
/// `expiresAt` when it has its own.
pub const QUEUE_DEFAULT_WINDOW_MS: f64 = 10.0 * 60.0 * 1000.0;

/// The reason a command whose delivery window closed before its turn is
/// answered with.
pub const EXPIRED_BEFORE_RUN: &str =
    "command expired: its delivery window closed before the node could run it";

/// One command-poll pass.
///
/// 1. A command a restart cut short (started in the record, never finished) is
///    answered `failed` ([`INTERRUPTED`](crate::dispatch::executed_commands::INTERRUPTED)):
///    it may have acted, so it never runs again.
/// 2. Every ack the queue does not have yet goes out before the claim, and the
///    pass claims nothing while one still fails to: the claim would fail that
///    ack's row as never acknowledged once its lease and window ran out. An
///    ack the queue answers with a retryable error is given up after
///    [`ACK_MAX_REFUSALS`] passes; acks the queue cannot be reached for, after
///    [`ACK_OUTBOX_MAX_HOLD`].
/// 3. Claim the queue and take each command in turn: an id already in the
///    executed record is answered with its stored ack and not run again; one
///    whose delivery window closed while earlier commands ran is answered
///    `failed` ([`EXPIRED_BEFORE_RUN`]) without running; the rest are recorded
///    as started, run, recorded with their ack, and acked.
///
/// Best-effort: any transport failure is logged, not fatal.
pub async fn poll_once<L: CommandLink, R: CommandRunner>(
    link: &L,
    runner: &R,
    device_id: &str,
    executed: &mut ExecutedCommands,
) {
    for id in executed.interrupted() {
        tracing::warn!(id = %id, "cloud command cut short by a restart; answered failed, not run again");
        let ack = build_ack(&id, device_id, &CommandResult::failed(INTERRUPTED));
        if let Err(e) = executed.record(&id, ack) {
            tracing::warn!(error = %e, "executed-command record write failed");
        }
    }
    if !flush_acks(link, executed).await {
        return;
    }
    let sent = Instant::now();
    let Some(body) = link.claim().await else {
        tracing::debug!("cloud command poll failed");
        return;
    };
    for cmd in parse_commands(&body) {
        let cmd_id = command_id(&cmd).to_string();
        let name = command_name(&cmd).to_string();

        let ack = match executed.precheck(&cmd) {
            Precheck::Replay(ack) => {
                tracing::info!(command = %name, id = %cmd_id, "cloud command already executed; re-sending its ack");
                ack
            }
            Precheck::Refuse(result) => {
                tracing::warn!(
                    command = %name,
                    id = %cmd_id,
                    reason = %result.result.get("message").and_then(|v| v.as_str()).unwrap_or(""),
                    "cloud command redelivered; refused"
                );
                let ack = build_ack(&cmd_id, device_id, &result);
                if let Err(e) = executed.record(&cmd_id, ack.clone()) {
                    tracing::warn!(error = %e, "executed-command record write failed");
                }
                ack
            }
            Precheck::Execute => {
                let result = if run_deadline(&cmd, sent).is_some_and(|d| Instant::now() >= d) {
                    tracing::warn!(command = %name, id = %cmd_id, "cloud command's delivery window closed before its turn; not run");
                    CommandResult::failed(EXPIRED_BEFORE_RUN)
                } else {
                    tracing::info!(command = %name, id = %cmd_id, "cloud command executing");
                    if let Err(e) = executed.begin(&cmd_id) {
                        tracing::warn!(error = %e, "executed-command record write failed");
                    }
                    runner.run(&name, &cmd).await
                };
                if result.status == CommandStatus::Failed {
                    tracing::warn!(
                        command = %name,
                        id = %cmd_id,
                        message = %result.result.get("message").and_then(|v| v.as_str()).unwrap_or(""),
                        "cloud command failed"
                    );
                }
                let ack = build_ack(&cmd_id, device_id, &result);
                if let Err(e) = executed.record(&cmd_id, ack.clone()) {
                    tracing::warn!(error = %e, "executed-command record write failed");
                }
                ack
            }
        };
        settle(executed, &cmd_id, post_ack(link, &ack).await);
    }
}

/// Book one ack's delivery pass in the record; true when the ack is still
/// held (it holds the claim back).
fn settle(executed: &mut ExecutedCommands, id: &str, fate: AckFate) -> bool {
    let held = match fate {
        AckFate::Settled => {
            if let Err(e) = executed.delivered(id) {
                tracing::warn!(error = %e, "executed-command record write failed");
            }
            false
        }
        AckFate::Refused => match executed.refused(id) {
            Ok(n) if n >= ACK_MAX_REFUSALS => {
                tracing::warn!(id = %id, refusals = n, "cloud command ack refused on every pass; given up");
                if let Err(e) = executed.delivered(id) {
                    tracing::warn!(error = %e, "executed-command record write failed");
                }
                false
            }
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(error = %e, "executed-command record write failed");
                true
            }
        },
        AckFate::Unanswered => true,
    };
    if held {
        tracing::warn!(id = %id, "cloud command ack not delivered; it goes out before the next claim");
    }
    held
}

/// Send every undelivered ack, oldest first; true when the claim may go
/// ahead. A refused ack is booked and the next one tried; an unanswered one
/// ends the flush (the queue is away). After [`ACK_OUTBOX_MAX_HOLD`] of the
/// queue being away, the held acks are dropped.
async fn flush_acks<L: CommandLink>(link: &L, executed: &mut ExecutedCommands) -> bool {
    let mut held = false;
    let mut away = false;
    for ack in executed.undelivered() {
        let fate = post_ack(link, &ack).await;
        let id = ack.get("commandId").and_then(|v| v.as_str()).unwrap_or("");
        held |= settle(executed, id, fate);
        if fate == AckFate::Unanswered {
            away = true;
            break;
        }
    }
    if !away {
        executed.outbox_stalled_since = None;
        return !held;
    }
    let since = *executed
        .outbox_stalled_since
        .get_or_insert_with(Instant::now);
    if since.elapsed() < ACK_OUTBOX_MAX_HOLD {
        tracing::debug!("cloud command acks still undelivered; not claiming");
        return false;
    }
    tracing::warn!("cloud command acks undelivered for an hour; dropped");
    if let Err(e) = executed.drop_undelivered() {
        tracing::warn!(error = %e, "executed-command record write failed");
    }
    executed.outbox_stalled_since = None;
    true
}

/// When a claimed row's delivery window closes, on this node's clock: the
/// window's end less the claim time the queue stamped, counted from when the
/// claim was `sent` (no later than the queue stamped it), so the node's clock
/// never has to agree with the queue's. `None` for a row without the stamps.
pub fn run_deadline(row: &serde_json::Value, sent: Instant) -> Option<Instant> {
    let ms = |k: &str| row.get(k).and_then(serde_json::Value::as_f64);
    let claimed = ms("claimedAt")?;
    let end = ms("expiresAt").or_else(|| Some(ms("createdAt")? + QUEUE_DEFAULT_WINDOW_MS))?;
    let left_ms = (end - claimed).max(0.0);
    Some(sent + Duration::from_millis(left_ms as u64))
}

/// How one ack's delivery pass ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckFate {
    /// The queue has it, or refused it for good (a 4xx other than 408 and
    /// 429: a row that is gone answers 404): never sent again.
    Settled,
    /// The last try was answered with a retryable error (5xx, 408, 429).
    Refused,
    /// The last try got no answer.
    Unanswered,
}

/// POST one command ack, retrying a transport error or a retryable answer.
pub async fn post_ack<L: CommandLink>(link: &L, ack: &serde_json::Value) -> AckFate {
    let mut fate = AckFate::Unanswered;
    for delay in ACK_RETRY_DELAYS {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        fate = match link.ack(ack).await {
            AckAttempt::Delivered => return AckFate::Settled,
            AckAttempt::Refused(status @ 400..=499) if status != 408 && status != 429 => {
                tracing::warn!(status, "cloud command ack refused by the queue; not resent");
                return AckFate::Settled;
            }
            AckAttempt::Refused(status) => {
                tracing::debug!(status, "cloud command ack refused");
                AckFate::Refused
            }
            AckAttempt::Unanswered => {
                tracing::debug!("cloud command ack failed");
                AckFate::Unanswered
            }
        };
    }
    fate
}

/// Build the ACK payload for a dispatched command. The shape is `{commandId, deviceId,
/// status, result?, data?}`. `result` is included when non-null; `data` when present.
pub fn build_ack(command_id: &str, device_id: &str, result: &CommandResult) -> serde_json::Value {
    let mut ack = serde_json::Map::new();
    ack.insert("commandId".to_string(), serde_json::json!(command_id));
    ack.insert("deviceId".to_string(), serde_json::json!(device_id));
    ack.insert(
        "status".to_string(),
        serde_json::json!(result.status.as_str()),
    );
    // The small result doc is always present here (the dispatcher always sets
    // it); include it under `result` matching the Python `if result:` branch.
    ack.insert("result".to_string(), result.result.clone());
    if let Some(data) = &result.data {
        ack.insert("data".to_string(), data.clone());
    }
    serde_json::Value::Object(ack)
}

/// Extract the command id from a command-queue row (`_id`). Mirrors
/// `cmd.get("_id")`.
pub fn command_id(row: &serde_json::Value) -> &str {
    row.get("_id").and_then(|v| v.as_str()).unwrap_or("")
}

/// Extract the command name from a row (`command`). Mirrors
/// `cmd.get("command", "unknown")`.
pub fn command_name(row: &serde_json::Value) -> &str {
    row.get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
}

/// Parse the `commands[]` array out of a `GET /agent/commands` response body.
pub fn parse_commands(body: &serde_json::Value) -> Vec<serde_json::Value> {
    body.get("commands")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
