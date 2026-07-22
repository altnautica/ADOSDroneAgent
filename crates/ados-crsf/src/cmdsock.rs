//! Operator command socket for the CRSF lane.
//!
//! Wire protocol (mirrors the other service command sockets): one
//! newline-terminated JSON request, one newline-terminated JSON response per
//! connection, then the server closes.
//!
//! ```text
//! {"op":"status"}
//!     -> {"ok":true, …the current sidecar body…}
//! {"op":"set_channels","channels":[992, …16 values…],"ttl_ms":1000,"client_id":"ai-1"}
//!     -> {"ok":true,"channels":[…],"ttl_ms":1000,"channel_source":"inject","authority":"…"}
//! {"op":"set_channel","index":4,"value":1811,"ttl_ms":1000,"client_id":"ai-1"}
//!     -> {"ok":true,"index":4,"value":1811,"ttl_ms":1000,"channel_source":"inject","authority":"…"}
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
//! Every injection carries a time-to-live (`ttl_ms`, clamped into the allowed
//! window, defaulting when absent): a silent injector's values decay to the
//! safe neutral set, never a held stale stick. `authority` in the reply names
//! the source the transmitter obeys RIGHT NOW — an injection accepted while
//! the HID path holds authority is stored but not transmitted, and the reply
//! says so honestly.
//!
//! Channel values are validated against the usable endpoint range 172..=1811
//! at parse time, before the live merge is ever locked; a failed request
//! replies `{"ok":false,"error":"E_…"}` and changes nothing.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_protocol::ipc::{bind_command_socket, serve_rpc};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::bank::BankError;
use crate::channels::{CHANNEL_COUNT, CHANNEL_MAX, CHANNEL_MIN};
use crate::frame::{ADDR_HANDSET, ADDR_TRANSMITTER_MODULE, MAX_PAYLOAD};
use crate::params::ParameterWrite;
use crate::sources::{clamp_ttl, SourceMerge, DEFAULT_INJECT_TTL};
use crate::transport::OobQueue;

/// Largest parameter value payload a single write frame can carry: the wire
/// payload ceiling minus the dest/origin/field-index header.
pub const MAX_PARAM_DATA: usize = MAX_PAYLOAD - ParameterWrite::HEADER_SIZE;

/// Cap on a single request line so a malformed client can't grow the buffer.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// The shared lane state the command handlers touch: the source merge (the
/// injection writer's target and the TX task's reader) and the latest sidecar
/// body the status verb serves. Both outlive a single serial bring-up.
#[derive(Clone)]
pub struct CmdState {
    pub merge: Arc<Mutex<SourceMerge>>,
    pub latest_status: Arc<Mutex<Value>>,
    /// The out-of-band frame lane the TX task drains between RC frames;
    /// parameter writes ride it.
    pub oob: Arc<OobQueue>,
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
    field_index: Option<u8>,
    #[serde(default)]
    data: Option<Vec<u8>>,
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
        client_id: Option<String>,
    },
    SetChannel {
        index: usize,
        value: u16,
        ttl: Duration,
        client_id: Option<String>,
    },
    ParamWrite {
        field_index: u8,
        data: Vec<u8>,
    },
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
                client_id: req.client_id,
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
                client_id: req.client_id,
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
            client_id,
        } => {
            let now = Instant::now();
            let mut merge = state.merge.lock().await;
            match merge.inject_all(channels, ttl, now, client_id) {
                Ok(()) => json!({
                    "ok": true,
                    "channels": channels.to_vec(),
                    "ttl_ms": ttl.as_millis() as u64,
                    "channel_source": crate::sources::ChannelSource::Inject.as_str(),
                    "authority": merge.authority(now).as_str(),
                }),
                Err(e) => bank_error_reply(e),
            }
        }
        Command::SetChannel {
            index,
            value,
            ttl,
            client_id,
        } => {
            let now = Instant::now();
            let mut merge = state.merge.lock().await;
            match merge.inject_one(index, value, ttl, now, client_id) {
                Ok(()) => json!({
                    "ok": true,
                    "index": index,
                    "value": value,
                    "ttl_ms": ttl.as_millis() as u64,
                    "channel_source": crate::sources::ChannelSource::Inject.as_str(),
                    "authority": merge.authority(now).as_str(),
                }),
                Err(e) => bank_error_reply(e),
            }
        }
        Command::ParamWrite { field_index, data } => {
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
mod tests {
    use super::*;
    use crate::sources::{ChannelSourceMode, MAX_INJECT_TTL, MIN_INJECT_TTL};

    fn state() -> CmdState {
        CmdState {
            merge: Arc::new(Mutex::new(SourceMerge::new(ChannelSourceMode::Inject))),
            latest_status: Arc::new(Mutex::new(Value::Null)),
            oob: Arc::new(OobQueue::default()),
        }
    }

    #[test]
    fn parse_rejects_malformed_json() {
        match parse_command(b"not json") {
            Parsed::Reply(v) => {
                assert_eq!(v["ok"], false);
                assert!(v["error"].as_str().unwrap().starts_with("E_BAD_REQUEST"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_unknown_op_and_missing_fields() {
        match parse_command(br#"{"op":"reboot"}"#) {
            Parsed::Reply(v) => assert_eq!(v["error"], "E_UNKNOWN_OP: reboot"),
            other => panic!("unexpected {other:?}"),
        }
        match parse_command(br#"{"op":"set_channels"}"#) {
            Parsed::Reply(v) => assert_eq!(v["error"], "E_MISSING_CHANNELS"),
            other => panic!("unexpected {other:?}"),
        }
        match parse_command(br#"{"op":"set_channel","index":1}"#) {
            Parsed::Reply(v) => assert_eq!(v["error"], "E_MISSING_VALUE"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_wrong_count_and_out_of_range_values() {
        match parse_command(br#"{"op":"set_channels","channels":[992, 992]}"#) {
            Parsed::Reply(v) => assert_eq!(v["error"], "E_BAD_CHANNEL_COUNT"),
            other => panic!("unexpected {other:?}"),
        }
        let mut chans = vec![992u16; 16];
        chans[3] = 2000;
        let line = serde_json::to_vec(&json!({"op":"set_channels","channels":chans})).unwrap();
        match parse_command(&line) {
            Parsed::Reply(v) => assert_eq!(v["error"], "E_BAD_CHANNEL_VALUE: 2000"),
            other => panic!("unexpected {other:?}"),
        }
        match parse_command(br#"{"op":"set_channel","index":16,"value":992}"#) {
            Parsed::Reply(v) => assert_eq!(v["error"], "E_BAD_CHANNEL_INDEX: 16"),
            other => panic!("unexpected {other:?}"),
        }
        match parse_command(br#"{"op":"set_channel","index":0,"value":171}"#) {
            Parsed::Reply(v) => assert_eq!(v["error"], "E_BAD_CHANNEL_VALUE: 171"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parse_accepts_valid_commands_with_ttl_and_client() {
        assert_eq!(
            parse_command(br#"{"op":"status"}"#),
            Parsed::Cmd(Command::Status)
        );
        assert_eq!(
            parse_command(
                br#"{"op":"set_channel","index":4,"value":1811,"ttl_ms":700,"client_id":"ai"}"#
            ),
            Parsed::Cmd(Command::SetChannel {
                index: 4,
                value: 1811,
                ttl: Duration::from_millis(700),
                client_id: Some("ai".to_string()),
            })
        );
        // Absent ttl defaults; out-of-window ttl clamps at parse time.
        match parse_command(br#"{"op":"set_channel","index":0,"value":992}"#) {
            Parsed::Cmd(Command::SetChannel { ttl, .. }) => {
                assert_eq!(ttl, DEFAULT_INJECT_TTL);
            }
            other => panic!("unexpected {other:?}"),
        }
        match parse_command(br#"{"op":"set_channel","index":0,"value":992,"ttl_ms":1}"#) {
            Parsed::Cmd(Command::SetChannel { ttl, .. }) => assert_eq!(ttl, MIN_INJECT_TTL),
            other => panic!("unexpected {other:?}"),
        }
        match parse_command(br#"{"op":"set_channel","index":0,"value":992,"ttl_ms":999999}"#) {
            Parsed::Cmd(Command::SetChannel { ttl, .. }) => assert_eq!(ttl, MAX_INJECT_TTL),
            other => panic!("unexpected {other:?}"),
        }
        let line =
            serde_json::to_vec(&json!({"op":"set_channels","channels":vec![992u16;16]})).unwrap();
        match parse_command(&line) {
            Parsed::Cmd(Command::SetChannels {
                channels,
                ttl,
                client_id,
            }) => {
                assert_eq!(channels, [992u16; 16]);
                assert_eq!(ttl, DEFAULT_INJECT_TTL);
                assert_eq!(client_id, None);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn apply_set_channels_updates_the_merge() {
        let st = state();
        let resp = dispatch(
            &serde_json::to_vec(&json!({"op":"set_channels","channels":vec![1000u16;16]})).unwrap(),
            &st,
        )
        .await;
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["channel_source"], "inject");
        assert_eq!(resp["authority"], "inject");
        assert_eq!(resp["ttl_ms"], DEFAULT_INJECT_TTL.as_millis() as u64);
        let (values, src) = st.merge.lock().await.current(Instant::now());
        assert_eq!(values, [1000u16; 16]);
        assert_eq!(src.map(|s| s.as_str()), Some("inject"));
    }

    #[tokio::test]
    async fn apply_set_channel_updates_one_slot() {
        let st = state();
        let resp = dispatch(br#"{"op":"set_channel","index":7,"value":1500}"#, &st).await;
        assert_eq!(resp["ok"], true);
        let (values, _) = st.merge.lock().await.current(Instant::now());
        assert_eq!(values[7], 1500);
    }

    #[tokio::test]
    async fn injection_under_hid_authority_reports_it() {
        // In hid mode the injection is stored but the HID path holds
        // authority; the reply must say so, never imply the values fly.
        let st = CmdState {
            merge: Arc::new(Mutex::new(SourceMerge::new(ChannelSourceMode::Hid))),
            latest_status: Arc::new(Mutex::new(Value::Null)),
            oob: Arc::new(OobQueue::default()),
        };
        let resp = dispatch(
            &serde_json::to_vec(&json!({"op":"set_channels","channels":vec![1000u16;16]})).unwrap(),
            &st,
        )
        .await;
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["authority"], "hid");
        // The transmitted set stays neutral (no HID data): the injection did
        // not leak through.
        let (values, src) = st.merge.lock().await.current(Instant::now());
        assert_eq!(values, crate::bank::ChannelBank::neutral());
        assert!(src.is_none());
    }

    #[test]
    fn parse_validates_param_write() {
        match parse_command(br#"{"op":"param_write"}"#) {
            Parsed::Reply(v) => assert_eq!(v["error"], "E_MISSING_FIELD_INDEX"),
            other => panic!("unexpected {other:?}"),
        }
        let oversize = serde_json::to_vec(
            &json!({"op":"param_write","field_index":1,"data":vec![0u8;MAX_PARAM_DATA+1]}),
        )
        .unwrap();
        match parse_command(&oversize) {
            Parsed::Reply(v) => assert!(v["error"]
                .as_str()
                .unwrap()
                .starts_with("E_PARAM_DATA_TOO_LARGE")),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            parse_command(br#"{"op":"param_write","field_index":3,"data":[2]}"#),
            Parsed::Cmd(Command::ParamWrite {
                field_index: 3,
                data: vec![2],
            })
        );
    }

    #[tokio::test]
    async fn param_write_queues_a_valid_wire_frame() {
        let st = state();
        let resp = dispatch(br#"{"op":"param_write","field_index":3,"data":[2]}"#, &st).await;
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["field_index"], 3);
        assert_eq!(resp["queued"], true);
        let frames = st.oob.drain();
        assert_eq!(frames.len(), 1);
        let (frame, _) = crate::frame::parse_frame(&frames[0]).unwrap();
        assert_eq!(frame.frame_type, crate::frame::TYPE_PARAMETER_WRITE);
        let decoded = ParameterWrite::decode(&frame.payload).unwrap();
        assert_eq!(decoded.dest, ADDR_TRANSMITTER_MODULE);
        assert_eq!(decoded.origin, ADDR_HANDSET);
        assert_eq!(decoded.field_index, 3);
        assert_eq!(decoded.data, vec![2]);
    }

    #[tokio::test]
    async fn param_write_reports_a_full_queue() {
        let st = state();
        for _ in 0..crate::transport::OOB_QUEUE_CAP {
            let resp = dispatch(br#"{"op":"param_write","field_index":1,"data":[0]}"#, &st).await;
            assert_eq!(resp["ok"], true);
        }
        let resp = dispatch(br#"{"op":"param_write","field_index":1,"data":[0]}"#, &st).await;
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"], "E_QUEUE_FULL");
    }

    #[tokio::test]
    async fn status_merges_the_latest_sidecar_body() {
        let st = state();
        // Before any heartbeat: honest null status.
        let resp = dispatch(br#"{"op":"status"}"#, &st).await;
        assert_eq!(resp["ok"], true);
        assert!(resp["status"].is_null());
        // After a heartbeat wrote a body: the body rides under ok:true.
        *st.latest_status.lock().await = json!({"state": "ready", "rssi_dbm": null});
        let resp = dispatch(br#"{"op":"status"}"#, &st).await;
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["state"], "ready");
        assert!(resp["rssi_dbm"].is_null());
    }

    #[tokio::test]
    async fn end_to_end_over_a_real_unix_socket() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("crsf-cmd.sock");
        let st = state();
        let server = tokio::spawn({
            let st = st.clone();
            let sock = sock.clone();
            async move {
                let _ = serve(st, &sock).await;
            }
        });
        // Wait for the socket to appear.
        for _ in 0..100 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let mut stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
        stream
            .write_all(b"{\"op\":\"set_channel\",\"index\":2,\"value\":992}\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let resp: Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(resp["ok"], true);
        let (values, _) = st.merge.lock().await.current(Instant::now());
        assert_eq!(values[2], 992);
        server.abort();
    }
}
