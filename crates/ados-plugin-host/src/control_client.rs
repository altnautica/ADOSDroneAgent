//! Synchronous client for the plugin host's on-box control socket.
//!
//! The lifecycle controller is synchronous (install / enable / grant are a few
//! hundred milliseconds of `systemctl` and filesystem work, like the rest of
//! the agent control plane) and the plugin host daemon is async. The wire
//! between them is a plain length-prefixed msgpack envelope, so the controller
//! does not need a runtime to reach it: a blocking `std::os::unix::net`
//! connection with a short timeout is the whole client.
//!
//! **Why the controller calls the daemon at all.** State on disk is not the
//! enforcement point. A plugin's capabilities live in a minted HMAC token the
//! daemon holds and in the sandbox of a unit systemd already exec'd, so a
//! controller that only writes state has changed nothing a running plugin can
//! observe. These two calls are how a write becomes effective:
//!
//! * [`reconcile`] before starting a plugin unit, so the socket is bound and
//!   the token env file written by the time the runner looks for them. Without
//!   it the runner found neither and the plugin ran inert.
//! * [`rotate_token`] after a grant or revoke, so the live session's token is
//!   re-minted from the new grant set and the next request re-gates.
//!
//! **Failing to reach the daemon is not an error the caller must abort on.**
//! The reconciler polls state on a fixed interval regardless, so an
//! unreachable socket costs at most one poll period of delay, not correctness.
//! Callers log the miss and carry on, which is why every function here returns
//! a plain `Result<_, String>` describing the miss rather than a typed error
//! anyone is expected to match on.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use ados_protocol::frame::{decode_len, HEADER_SIZE, PLUGIN_MAX_FRAME};
use ados_protocol::plugin::{Envelope, PROTOCOL_VERSION};
use rmpv::Value;

use crate::control::{control_socket_path, METHOD_PLUGIN_RECONCILE, METHOD_TOKEN_ROTATE};

/// Timeout for one control round trip. Generous against a loaded SBC, short
/// enough that a wedged daemon cannot stall an operator's CLI call.
const TIMEOUT: Duration = Duration::from_secs(3);

/// Ask the daemon to reconcile its served sockets against plugin state.
///
/// Call this *before* `systemctl start` of a plugin unit: the socket and the
/// token env file must exist before the runner looks for them. `control_dir` is
/// the daemon's control dir ([`crate::control::DEFAULT_CONTROL_DIR`]).
pub fn reconcile(control_dir: &Path) -> Result<(), String> {
    request(control_dir, METHOD_PLUGIN_RECONCILE, Value::Map(vec![])).map(|_| ())
}

/// Ask the daemon to re-mint `plugin_id`'s capability token from the current
/// grant set and push it into the plugin's live session.
pub fn rotate_token(control_dir: &Path, plugin_id: &str) -> Result<(), String> {
    request(
        control_dir,
        METHOD_TOKEN_ROTATE,
        Value::Map(vec![(Value::from("plugin_id"), Value::from(plugin_id))]),
    )
    .map(|_| ())
}

/// One request/response round trip. Returns the response args on success.
fn request(control_dir: &Path, method: &str, args: Value) -> Result<Value, String> {
    let path = control_socket_path(control_dir);
    let mut stream =
        UnixStream::connect(&path).map_err(|e| format!("connect {}: {e}", path.display()))?;
    stream
        .set_read_timeout(Some(TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(TIMEOUT)))
        .map_err(|e| format!("set timeouts on {}: {e}", path.display()))?;

    let env = Envelope {
        version: PROTOCOL_VERSION,
        kind: "request".to_string(),
        method: method.to_string(),
        capability: String::new(),
        args,
        request_id: format!("ctl-{method}"),
        token: String::new(),
        error: None,
    };
    let frame = env
        .encode_frame()
        .map_err(|e| format!("encode {method}: {e}"))?;
    stream
        .write_all(&frame)
        .and_then(|()| stream.flush())
        .map_err(|e| format!("write {method}: {e}"))?;

    let mut header = [0u8; HEADER_SIZE];
    stream
        .read_exact(&mut header)
        .map_err(|e| format!("read {method} header: {e}"))?;
    let len = decode_len(header, PLUGIN_MAX_FRAME, true)
        .map_err(|e| format!("decode {method} length: {e}"))?;
    let mut body = vec![0u8; len];
    stream
        .read_exact(&mut body)
        .map_err(|e| format!("read {method} body: {e}"))?;
    let resp =
        Envelope::from_msgpack(&body).map_err(|e| format!("decode {method} response: {e}"))?;
    match resp.error {
        Some(msg) => Err(msg),
        None => Ok(resp.args),
    }
}
