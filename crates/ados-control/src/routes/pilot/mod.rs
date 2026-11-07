//! The operator side of the command authority.
//!
//! * `WS /api/v1/pilot/stream` (ticket scope [`SCOPE_PILOT_CONTROL`]): a
//!   surface's sticks, actions and session ops in, the pilot's state out.
//!   Client frames are the `pilot.v1` requests as JSON text; server frames are
//!   `{kind:"pilot", ts_ms, data}` at 10 Hz, `{kind:"action_result", name, id?,
//!   ok, err?}` per action and `{kind:"keepalive", ts_ms}` once a second when
//!   nothing else was sent. MCP tokens never reach this scope.
//! * The nav ingest: the pilot's `state` and `obstacles` become the nav-stream
//!   kinds `pilot` and `obstacles`, and the nav hub's view demand goes back to
//!   the pilot so it projects obstacles only for watched cameras.

pub mod command;
pub mod ingest;
mod stream;

use std::path::PathBuf;

use ados_protocol::pairing_posture::CallerClass;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Extension, State};
use axum::http::HeaderMap;
use axum::response::Response;

use crate::routes::gs_ws::{caller_of, decide_ws_auth, upgrade_with, ws_reject};
use crate::state::AppState;

/// The ticket scope a `/api/v1/pilot/stream` upgrade must carry.
pub const SCOPE_PILOT_CONTROL: &str = "pilot.control";

/// The pilot daemon's socket.
pub fn pilot_socket() -> PathBuf {
    ados_protocol::paths::run_dir().join(ados_protocol::paths::PILOT_SOCK)
}

/// `WS /api/v1/pilot/stream`, authenticated like the nav stream.
pub async fn ws_pilot_stream(
    State(state): State<AppState>,
    headers: HeaderMap,
    caller: Option<Extension<CallerClass>>,
    ws: WebSocketUpgrade,
) -> Response {
    let auth = decide_ws_auth(&state, &headers, SCOPE_PILOT_CONTROL, caller_of(caller));
    let Some((ws, _auth)) = upgrade_with(ws, auth) else {
        return ws_reject();
    };
    ws.on_upgrade(move |socket| stream::pilot_stream_loop(socket, pilot_socket()))
}
