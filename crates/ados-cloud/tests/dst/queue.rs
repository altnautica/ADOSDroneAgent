//! The simulated cloud command queue: the backend's claim and ack mutations,
//! transcribed rule for rule. A claim leases each deliverable row (pending, or
//! delivering past its lease) oldest first, at most a batch at a time, with a
//! fresh `claimedAt` and one more attempt; a row past its delivery window is
//! failed as expired instead, and one past its attempt budget as
//! undeliverable. An ack closes a row still awaiting its verdict; a terminal
//! row keeps the verdict it has and the late ack is answered `ignored`.

use serde_json::{json, Value};

/// How long a claim leases a row.
pub const CLAIM_LEASE_MS: u64 = 60_000;
/// Claims a row may take before it is failed as undeliverable.
pub const MAX_DELIVERY_ATTEMPTS: u32 = 5;
/// Rows handed out per claim.
pub const MAX_DELIVERY_BATCH: usize = 25;
/// Delivery window of a row queued with no TTL.
pub const DEFAULT_DELIVERY_WINDOW_MS: u64 = 10 * 60 * 1000;
/// Delivery window of a flight command queued with no TTL.
pub const FLIGHT_COMMAND_TTL_MS: u64 = 10_000;
/// Bounds a queued TTL is clamped to.
pub const MIN_COMMAND_TTL_MS: u64 = 1_000;
pub const MAX_COMMAND_TTL_MS: u64 = 60_000;

/// The server clock's epoch at simulated time zero (ms).
pub const EPOCH_MS: u64 = 1_763_600_000_000;

pub const DEVICE: &str = "dst-node-1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pending,
    Delivering,
    Completed,
    Failed,
}

impl Status {
    pub fn terminal(self) -> bool {
        matches!(self, Status::Completed | Status::Failed)
    }
}

/// One queue row.
#[derive(Clone, Debug)]
pub struct Row {
    pub id: String,
    pub command: &'static str,
    pub args: Value,
    /// Server ms.
    pub created_at: u64,
    pub expires_at: Option<u64>,
    pub status: Status,
    pub claimed_at: Option<u64>,
    pub attempts: u32,
    pub delivered_at: Option<u64>,
    /// The verdict's message once terminal.
    pub message: Option<String>,
    /// Whether an agent ack set the verdict (else the queue did).
    pub closed_by_ack: bool,
}

impl Row {
    /// The end of the row's delivery window (server ms).
    pub fn deadline(&self) -> u64 {
        self.expires_at
            .unwrap_or(self.created_at + DEFAULT_DELIVERY_WINDOW_MS)
    }

    fn wire(&self) -> Value {
        let mut row = json!({
            "_id": self.id,
            "_creationTime": self.created_at,
            "deviceId": DEVICE,
            "userId": "dst-user",
            "command": self.command,
            "status": "delivering",
            "createdAt": self.created_at,
            "claimedAt": self.claimed_at,
            "attempts": self.attempts,
            "deliveredAt": self.delivered_at,
        });
        if !self.args.is_null() {
            row["args"] = self.args.clone();
        }
        if let Some(e) = self.expires_at {
            row["expiresAt"] = json!(e);
        }
        row
    }
}

/// Whether a command is flight-class (it gets a delivery window regardless).
pub fn is_flight(command: &str) -> bool {
    command == "send_command"
}

#[derive(Default)]
pub struct Queue {
    pub rows: Vec<Row>,
}

impl Queue {
    /// Queue a command at server time `now`; returns its id.
    pub fn enqueue(
        &mut self,
        now: u64,
        command: &'static str,
        args: Value,
        ttl_ms: Option<u64>,
    ) -> String {
        let id = format!("c{:03}", self.rows.len() + 1);
        let window = ttl_ms.or(is_flight(command).then_some(FLIGHT_COMMAND_TTL_MS));
        let expires_at = window.map(|w| now + w.clamp(MIN_COMMAND_TTL_MS, MAX_COMMAND_TTL_MS));
        self.rows.push(Row {
            id: id.clone(),
            command,
            args,
            created_at: now,
            expires_at,
            status: Status::Pending,
            claimed_at: None,
            attempts: 0,
            delivered_at: None,
            message: None,
            closed_by_ack: false,
        });
        id
    }

    pub fn row(&self, id: &str) -> Option<&Row> {
        self.rows.iter().find(|r| r.id == id)
    }

    /// The claim mutation at server time `now`: the rows leased, and the
    /// rows it failed (id, reason) on the way.
    pub fn claim(&mut self, now: u64) -> (Vec<Value>, Vec<(String, String)>) {
        let mut candidates: Vec<usize> = (0..self.rows.len())
            .filter(|&i| {
                let r = &self.rows[i];
                r.status == Status::Pending
                    || (r.status == Status::Delivering
                        && r.claimed_at.unwrap_or(0) + CLAIM_LEASE_MS <= now)
            })
            .collect();
        candidates.sort_by_key(|&i| (self.rows[i].created_at, i));
        candidates.truncate(MAX_DELIVERY_BATCH);
        let mut leased = Vec::new();
        let mut failed = Vec::new();
        for i in candidates {
            let r = &mut self.rows[i];
            if r.deadline() <= now {
                let reason = if r.status == Status::Pending {
                    "command expired: not delivered to the node in time"
                } else {
                    "command expired: claimed by the node but never acknowledged"
                };
                r.status = Status::Failed;
                r.message = Some(reason.to_string());
                failed.push((r.id.clone(), reason.to_string()));
                continue;
            }
            let attempts = r.attempts + 1;
            if attempts > MAX_DELIVERY_ATTEMPTS {
                let reason = "command undeliverable: delivery attempts exhausted";
                r.status = Status::Failed;
                r.message = Some(reason.to_string());
                failed.push((r.id.clone(), reason.to_string()));
                continue;
            }
            r.status = Status::Delivering;
            r.claimed_at = Some(now);
            r.attempts = attempts;
            r.delivered_at.get_or_insert(now);
            leased.push(r.wire());
        }
        (leased, failed)
    }

    /// The ack route and mutation: the HTTP status, and whether the ack set
    /// the row's verdict.
    pub fn ack(&mut self, body: &Value) -> (u16, bool) {
        let id = body.get("commandId").and_then(Value::as_str).unwrap_or("");
        let device = body.get("deviceId").and_then(Value::as_str).unwrap_or("");
        let status = match body.get("status").and_then(Value::as_str) {
            Some("completed") => Status::Completed,
            Some("failed") => Status::Failed,
            _ => return (400, false),
        };
        if id.is_empty() || device != DEVICE {
            return (400, false);
        }
        let Some(r) = self.rows.iter_mut().find(|r| r.id == id) else {
            return (500, false);
        };
        if r.status.terminal() {
            return (200, false);
        }
        let message = body
            .get("result")
            .and_then(|r| r.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        r.status = status;
        r.message = Some(message);
        r.closed_by_ack = true;
        (200, true)
    }
}
