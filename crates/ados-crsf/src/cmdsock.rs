//! Operator command socket for the CRSF lane.
//!
//! Wire protocol (mirrors the other service command sockets): one
//! newline-terminated JSON request, one newline-terminated JSON response per
//! connection, then the server closes.
//!
//! ```text
//! {"op":"status"}
//!     -> {"ok":true, …the current sidecar body…}
//! {"op":"set_channels","channels":[992, …16 values…],"ttl_ms":1000,
//!  "client_id":"ai-1","client_ticket":"v1|crsf.inject:ai-1|…","module":"m1"}
//!     -> {"ok":true,"channels":[…],"ttl_ms":1000,"channel_source":"inject","authority":"…","module":"m1"}
//! {"op":"set_channel","index":4,"value":1811,"ttl_ms":1000,"client_id":"ai-1"}
//!     -> {"ok":true,"index":4,"value":1811,"ttl_ms":1000,"channel_source":"inject","authority":"…","module":"m0"}
//! {"op":"param_write","field_index":3,"data":[2]}
//!     -> {"ok":true,"field_index":3,"queued":true}
//! ```
//!
//! `param_write` frames the RC module's configuration parameter write (the
//! packet-rate / TX-power / telemetry-ratio surface) and queues it on the
//! out-of-band lane the transmitter drains between RC frames — the lane is a
//! transparent carrier of the raw value bytes, exactly like the parameter
//! codec itself. `queued:true` means the frame is on the wire lane, not that
//! the module acknowledged it; the module's response (a settings entry)
//! arrives on the telemetry stream.
//!
//! `module` addresses one RC module of the lane by id. Absent, or naming the
//! pilot module, the injection lands on the pilot lane's merge (the configured
//! channel-source authority). Naming an `autonomy` module lands it on that
//! module's inject-only merge. Any other id is refused with
//! `E_UNKNOWN_MODULE`. The reply echoes the module the injection landed on.
//! The socket is served for the whole service pass, so each module is
//! addressable on its own; a write (injection or `param_write`, which targets
//! the pilot module) to a module whose serial port is down is refused with
//! `E_MODULE_DISCONNECTED`.
//!
//! Every injection carries a time-to-live (`ttl_ms`, clamped into the allowed
//! window, defaulting when absent): a silent injector stops feeding the lane
//! (nothing is transmitted), never a held stale stick. `authority` in the reply names
//! the source the transmitter obeys RIGHT NOW — an injection accepted while
//! the HID path holds authority is stored but not transmitted, and the reply
//! says so honestly.
//!
//! Channel values are validated against the usable endpoint range 172..=1811
//! at parse time, before the live merge is ever locked; a failed request
//! replies `{"ok":false,"error":"E_…"}` and changes nothing.
//!
//! ## Who the injector is
//!
//! `client_id` is a label. It is logged and echoed and nothing else rests on
//! it, because a caller writes it into its own request and this socket is
//! reachable by anything in the agent group. `client_ticket` is what the
//! hybrid authority rule reads: a signed attestation of that same id, minted
//! against the node pairing key by the authenticated route an operator or an
//! autonomous client actually goes through. An injection with no valid ticket
//! is still accepted and still stored — it simply establishes no identity, so
//! it cannot match the pilot-in-command holder and cannot take the lane from
//! a human. Naming the holder is no longer enough to become them.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_protocol::ipc::{bind_command_socket, serve_rpc};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::injector::InjectorAuth;
use crate::sources::{clamp_ttl, SourceMerge, DEFAULT_INJECT_TTL};
use crate::transport::OobQueue;
use ados_crsf_codec::bank::BankError;
use ados_crsf_codec::channels::{CHANNEL_COUNT, CHANNEL_MAX, CHANNEL_MIN};
use ados_crsf_codec::frame::{ADDR_HANDSET, ADDR_TRANSMITTER_MODULE, MAX_PAYLOAD};
use ados_crsf_codec::params::ParameterWrite;

/// Largest parameter value payload a single write frame can carry: the wire
/// payload ceiling minus the dest/origin/field-index header.
pub const MAX_PARAM_DATA: usize = MAX_PAYLOAD - ParameterWrite::HEADER_SIZE;

/// Cap on a single request line so a malformed client can't grow the buffer.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// The shared lane state the command handlers touch: the source merge (the
/// injection writer's target and the TX task's reader) and the latest sidecar
/// body the status verb serves. All of it outlives a single serial bring-up:
/// the socket is served for the whole service pass, so an autonomy module stays
/// addressable while the pilot module's port is down.
#[derive(Clone)]
pub struct CmdState {
    pub merge: Arc<Mutex<SourceMerge>>,
    pub latest_status: Arc<Mutex<Value>>,
    /// The pilot module's out-of-band frame lane the TX task drains between
    /// RC frames; parameter writes ride it. Emptied at every bring-up so a
    /// frame queued for a dead port never fires on a fresh one.
    pub oob: Arc<OobQueue>,
    /// Turns a claimed injector name into an attested one, or into nothing.
    pub injector_auth: Arc<InjectorAuth>,
    /// The lane's module addressing for channel injection.
    pub modules: Arc<ModuleTargets>,
}

/// One autonomy module as the command socket addresses it.
pub struct ModuleTarget {
    pub id: String,
    /// Its inject-only merge.
    pub merge: Arc<Mutex<SourceMerge>>,
    /// True while its serial transport is open.
    pub connected: Arc<AtomicBool>,
}

/// Where an injection addressed by module id lands: the pilot module (its
/// merge is [`CmdState::merge`]) and each autonomy module. A module whose
/// transport is down refuses writes with `E_MODULE_DISCONNECTED`: nothing
/// would carry them, and accepting them would read as a live lane.
pub struct ModuleTargets {
    pub pilot_id: Option<String>,
    /// True while the pilot module's serial transport is open.
    pub pilot_connected: Arc<AtomicBool>,
    pub autonomy: Vec<ModuleTarget>,
}

fn disconnected_reply(id: Option<&str>) -> Value {
    error_reply(format!("E_MODULE_DISCONNECTED: {}", id.unwrap_or("pilot")))
}

impl ModuleTargets {
    /// Refuse a pilot-lane write while the pilot transport is down.
    fn pilot_ready(&self) -> Result<(), Value> {
        if self.pilot_connected.load(Ordering::Relaxed) {
            Ok(())
        } else {
            Err(disconnected_reply(self.pilot_id.as_deref()))
        }
    }

    /// The merge an injection addressed to `module` lands on, with the module
    /// id to echo. `Err` carries the refusal reply for an unknown id or a
    /// module whose transport is down.
    fn target<'a>(
        &'a self,
        pilot: &'a Arc<Mutex<SourceMerge>>,
        module: Option<&str>,
    ) -> Result<(&'a Arc<Mutex<SourceMerge>>, Option<&'a str>), Value> {
        let pilot_id = self.pilot_id.as_deref();
        match module {
            Some(id) if Some(id) != pilot_id => {
                let target = self
                    .autonomy
                    .iter()
                    .find(|t| t.id == id)
                    .ok_or_else(|| error_reply(format!("E_UNKNOWN_MODULE: {id}")))?;
                if !target.connected.load(Ordering::Relaxed) {
                    return Err(disconnected_reply(Some(&target.id)));
                }
                Ok((&target.merge, Some(target.id.as_str())))
            }
            _ => {
                self.pilot_ready()?;
                Ok((pilot, pilot_id))
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct Request {
    op: String,
    #[serde(default)]
    channels: Option<Vec<u16>>,
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    value: Option<u16>,
    #[serde(default)]
    ttl_ms: Option<u64>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_ticket: Option<String>,
    #[serde(default)]
    field_index: Option<u8>,
    #[serde(default)]
    data: Option<Vec<u8>>,
    #[serde(default)]
    module: Option<String>,
}

/// Bind the command socket and serve one-shot requests until the listener
/// errors. Run as its own task from the service main loop.
pub async fn serve(state: CmdState, sock_path: &Path) -> std::io::Result<()> {
    let listener = bind_command_socket(sock_path, 0o660)?;
    tracing::info!(path = %sock_path.display(), "crsf command socket listening");

    serve_rpc(listener, MAX_REQUEST_BYTES, move |req: Vec<u8>| {
        let state = state.clone();
        async move {
            let resp = dispatch(&req, &state).await;
            serde_json::to_vec(&resp)
                .unwrap_or_else(|_| br#"{"ok":false,"error":"E_ENCODE"}"#.to_vec())
        }
    })
    .await;
    Ok(())
}

/// A request that has been parsed + field-validated and is ready to apply.
/// Parsing this OUT of the raw bytes is pure (no lane access), so every
/// malformed-request rejection happens before the service ever locks the
/// live merge.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Status,
    SetChannels {
        channels: [u16; CHANNEL_COUNT],
        ttl: Duration,
        claim: InjectorClaim,
        module: Option<String>,
    },
    SetChannel {
        index: usize,
        value: u16,
        ttl: Duration,
        claim: InjectorClaim,
        module: Option<String>,
    },
    ParamWrite {
        field_index: u8,
        data: Vec<u8>,
    },
}

/// What a request said about who is injecting: the label, and the attestation
/// that may turn it into an identity. Parsed together and resolved together so
/// no call site can reach for the label where the identity belongs.
#[derive(Debug, Default, PartialEq, Eq)]
struct InjectorClaim {
    client_id: Option<String>,
    ticket: Option<String>,
}

impl InjectorClaim {
    /// The attested identity, or `None`. A claimed name that fails to verify is
    /// logged rather than rejected: the injection is still valid input, it just
    /// does not carry an identity, so it cannot win the lane from a human.
    fn resolve(&self, auth: &InjectorAuth) -> Option<String> {
        let verified = auth.verify(self.client_id.as_deref(), self.ticket.as_deref());
        if verified.is_none() && self.client_id.is_some() {
            tracing::warn!(
                claimed = self.client_id.as_deref().unwrap_or_default(),
                had_ticket = self.ticket.is_some(),
                "crsf_injector_identity_unattested"
            );
        }
        verified
    }
}

#[derive(Debug, PartialEq)]
enum Parsed {
    Cmd(Command),
    Reply(Value),
}

fn error_reply(message: impl Into<String>) -> Value {
    json!({"ok": false, "error": message.into()})
}

fn channel_in_range(value: u16) -> bool {
    (CHANNEL_MIN..=CHANNEL_MAX).contains(&value)
}

/// Resolve the requested TTL: clamped into the allowed window, defaulting
/// when absent.
fn requested_ttl(ttl_ms: Option<u64>) -> Duration {
    match ttl_ms {
        Some(ms) => clamp_ttl(Duration::from_millis(ms)),
        None => DEFAULT_INJECT_TTL,
    }
}

/// Parse + validate one request line. Pure: no I/O, no locks.
fn parse_command(line: &[u8]) -> Parsed {
    let req: Request = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(e) => return Parsed::Reply(error_reply(format!("E_BAD_REQUEST: {e}"))),
    };
    match req.op.as_str() {
        "status" => Parsed::Cmd(Command::Status),
        "set_channels" => {
            let Some(channels) = req.channels else {
                return Parsed::Reply(error_reply("E_MISSING_CHANNELS"));
            };
            let channels: [u16; CHANNEL_COUNT] = match channels.try_into() {
                Ok(c) => c,
                Err(_) => return Parsed::Reply(error_reply("E_BAD_CHANNEL_COUNT")),
            };
            if let Some(bad) = channels.iter().find(|&&v| !channel_in_range(v)) {
                return Parsed::Reply(error_reply(format!("E_BAD_CHANNEL_VALUE: {bad}")));
            }
            Parsed::Cmd(Command::SetChannels {
                channels,
                ttl: requested_ttl(req.ttl_ms),
                claim: InjectorClaim {
                    client_id: req.client_id,
                    ticket: req.client_ticket,
                },
                module: req.module,
            })
        }
        "set_channel" => {
            let Some(index) = req.index else {
                return Parsed::Reply(error_reply("E_MISSING_INDEX"));
            };
            let Some(value) = req.value else {
                return Parsed::Reply(error_reply("E_MISSING_VALUE"));
            };
            if index >= CHANNEL_COUNT {
                return Parsed::Reply(error_reply(format!("E_BAD_CHANNEL_INDEX: {index}")));
            }
            if !channel_in_range(value) {
                return Parsed::Reply(error_reply(format!("E_BAD_CHANNEL_VALUE: {value}")));
            }
            Parsed::Cmd(Command::SetChannel {
                index,
                value,
                ttl: requested_ttl(req.ttl_ms),
                claim: InjectorClaim {
                    client_id: req.client_id,
                    ticket: req.client_ticket,
                },
                module: req.module,
            })
        }
        "param_write" => {
            let Some(field_index) = req.field_index else {
                return Parsed::Reply(error_reply("E_MISSING_FIELD_INDEX"));
            };
            let data = req.data.unwrap_or_default();
            if data.len() > MAX_PARAM_DATA {
                return Parsed::Reply(error_reply(format!(
                    "E_PARAM_DATA_TOO_LARGE: {} > {MAX_PARAM_DATA}",
                    data.len()
                )));
            }
            Parsed::Cmd(Command::ParamWrite { field_index, data })
        }
        other => Parsed::Reply(error_reply(format!("E_UNKNOWN_OP: {other}"))),
    }
}

async fn dispatch(line: &[u8], state: &CmdState) -> Value {
    match parse_command(line) {
        Parsed::Reply(v) => v,
        Parsed::Cmd(cmd) => apply(cmd, state).await,
    }
}

/// Apply a validated command to the live lane state.
async fn apply(cmd: Command, state: &CmdState) -> Value {
    match cmd {
        Command::Status => {
            let body = state.latest_status.lock().await.clone();
            match body {
                Value::Object(map) => {
                    let mut out = serde_json::Map::with_capacity(map.len() + 1);
                    out.insert("ok".to_string(), Value::Bool(true));
                    out.extend(map);
                    Value::Object(out)
                }
                // No heartbeat has run yet: report that honestly rather than
                // inventing a body.
                _ => json!({"ok": true, "status": Value::Null}),
            }
        }
        Command::SetChannels {
            channels,
            ttl,
            claim,
            module,
        } => {
            let (target, module_id) = match state.modules.target(&state.merge, module.as_deref()) {
                Ok(t) => t,
                Err(reply) => return reply,
            };
            let verified = claim.resolve(&state.injector_auth);
            let now = Instant::now();
            let mut merge = target.lock().await;
            match merge.inject_all(channels, ttl, now, verified) {
                Ok(()) => json!({
                    "ok": true,
                    "channels": channels.to_vec(),
                    "ttl_ms": ttl.as_millis() as u64,
                    "channel_source": crate::sources::ChannelSource::Inject.as_str(),
                    "authority": merge.authority(now).as_str(),
                    "module": module_id,
                }),
                Err(e) => bank_error_reply(e),
            }
        }
        Command::SetChannel {
            index,
            value,
            ttl,
            claim,
            module,
        } => {
            let (target, module_id) = match state.modules.target(&state.merge, module.as_deref()) {
                Ok(t) => t,
                Err(reply) => return reply,
            };
            let verified = claim.resolve(&state.injector_auth);
            let now = Instant::now();
            let mut merge = target.lock().await;
            match merge.inject_one(index, value, ttl, now, verified) {
                Ok(()) => json!({
                    "ok": true,
                    "index": index,
                    "value": value,
                    "ttl_ms": ttl.as_millis() as u64,
                    "channel_source": crate::sources::ChannelSource::Inject.as_str(),
                    "authority": merge.authority(now).as_str(),
                    "module": module_id,
                }),
                Err(e) => bank_error_reply(e),
            }
        }
        Command::ParamWrite { field_index, data } => {
            if let Err(reply) = state.modules.pilot_ready() {
                return reply;
            }
            // Frame the write toward the RC module (handset-originated) and
            // queue it on the out-of-band lane; the transmitter drains it
            // after the next RC frame.
            let write = ParameterWrite {
                dest: ADDR_TRANSMITTER_MODULE,
                origin: ADDR_HANDSET,
                field_index,
                data,
            };
            let frame = match write.to_frame(ADDR_TRANSMITTER_MODULE) {
                Ok(f) => f,
                // Unreachable through the parse-time size gate; loud if the
                // invariant ever breaks.
                Err(e) => return error_reply(format!("E_PARAM_FRAME: {e}")),
            };
            match state.oob.push(frame) {
                Ok(()) => json!({
                    "ok": true,
                    "field_index": field_index,
                    "queued": true,
                }),
                Err(_) => error_reply("E_QUEUE_FULL"),
            }
        }
    }
}

fn bank_error_reply(e: BankError) -> Value {
    match e {
        BankError::BadIndex(i) => error_reply(format!("E_BAD_CHANNEL_INDEX: {i}")),
        BankError::BadValue(v) => error_reply(format!("E_BAD_CHANNEL_VALUE: {v}")),
    }
}

#[cfg(test)]
mod tests;
