//! Operator command socket for the CRSF lane.
//!
//! Wire protocol (mirrors the other service command sockets): one
//! newline-terminated JSON request, one newline-terminated JSON response per
//! connection, then the server closes.
//!
//! ```text
//! {"op":"status"}
//!     -> {"ok":true, …the current sidecar body…}
//! {"op":"set_channels","channels":[992, …16 values…]}
//!     -> {"ok":true,"channels":[…],"channel_source":"api"}
//! {"op":"set_channel","index":4,"value":1811}
//!     -> {"ok":true,"index":4,"value":1811,"channel_source":"api"}
//! ```
//!
//! Channel values are validated against the usable endpoint range 172..=1811
//! at parse time, before the live bank is ever locked; a failed request
//! replies `{"ok":false,"error":"E_…"}` and changes nothing.

use std::path::Path;
use std::sync::Arc;

use ados_protocol::ipc::{bind_command_socket, serve_rpc};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::bank::{BankError, ChannelBank, ChannelSource};
use crate::channels::{CHANNEL_COUNT, CHANNEL_MAX, CHANNEL_MIN};

/// Cap on a single request line so a malformed client can't grow the buffer.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// The shared lane state the command handlers touch: the live channel bank
/// (written here, read by the transmit tick) and the latest sidecar body
/// (written by the heartbeat, served by the status verb). Both outlive a
/// single serial bring-up.
#[derive(Clone)]
pub struct CmdState {
    pub bank: Arc<Mutex<ChannelBank>>,
    pub latest_status: Arc<Mutex<Value>>,
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
/// live bank.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Status,
    SetChannels { channels: [u16; CHANNEL_COUNT] },
    SetChannel { index: usize, value: u16 },
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
            Parsed::Cmd(Command::SetChannels { channels })
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
            Parsed::Cmd(Command::SetChannel { index, value })
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
        Command::SetChannels { channels } => {
            let mut bank = state.bank.lock().await;
            match bank.set_all(channels, ChannelSource::Api) {
                Ok(()) => json!({
                    "ok": true,
                    "channels": bank.values().to_vec(),
                    "channel_source": ChannelSource::Api.as_str(),
                }),
                Err(e) => bank_error_reply(e),
            }
        }
        Command::SetChannel { index, value } => {
            let mut bank = state.bank.lock().await;
            match bank.set_one(index, value, ChannelSource::Api) {
                Ok(()) => json!({
                    "ok": true,
                    "index": index,
                    "value": value,
                    "channel_source": ChannelSource::Api.as_str(),
                }),
                Err(e) => bank_error_reply(e),
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

    fn state() -> CmdState {
        CmdState {
            bank: Arc::new(Mutex::new(ChannelBank::default())),
            latest_status: Arc::new(Mutex::new(Value::Null)),
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
    fn parse_accepts_valid_commands() {
        assert_eq!(
            parse_command(br#"{"op":"status"}"#),
            Parsed::Cmd(Command::Status)
        );
        assert_eq!(
            parse_command(br#"{"op":"set_channel","index":4,"value":1811}"#),
            Parsed::Cmd(Command::SetChannel {
                index: 4,
                value: 1811
            })
        );
        let line =
            serde_json::to_vec(&json!({"op":"set_channels","channels":vec![992u16;16]})).unwrap();
        match parse_command(&line) {
            Parsed::Cmd(Command::SetChannels { channels }) => {
                assert_eq!(channels, [992u16; 16]);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn apply_set_channels_updates_the_bank_and_source() {
        let st = state();
        let resp = dispatch(
            &serde_json::to_vec(&json!({"op":"set_channels","channels":vec![1000u16;16]})).unwrap(),
            &st,
        )
        .await;
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["channel_source"], "api");
        let bank = st.bank.lock().await;
        assert_eq!(bank.values(), [1000u16; 16]);
        assert_eq!(bank.source().map(|s| s.as_str()), Some("api"));
    }

    #[tokio::test]
    async fn apply_set_channel_updates_one_slot() {
        let st = state();
        let resp = dispatch(br#"{"op":"set_channel","index":7,"value":1500}"#, &st).await;
        assert_eq!(resp["ok"], true);
        assert_eq!(st.bank.lock().await.values()[7], 1500);
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
        assert_eq!(st.bank.lock().await.values()[2], 992);
        server.abort();
    }
}
