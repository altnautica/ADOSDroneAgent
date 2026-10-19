//! `video.whep.offer`, `video.whep.close` and `sensors.rig.write`: the relays a
//! sandboxed plugin cannot make itself.
//!
//! The plugin's sandbox closes loopback and hides the run directory. The host
//! runs outside it, so it carries the exchange:
//!
//! * WHEP goes straight to the media server's loopback WHEP endpoint for the
//!   requested leg (`127.0.0.1:<webrtc port>/<leg>/whep`), the same upstream
//!   the agent's own `/whep` front relays to. The session location handed back
//!   is the front's form (`/whep/<session>?camera=<leg>`), never an address,
//!   and is remembered as the calling plugin's: a close names one of its own
//!   locations or is refused.
//! * The rig goes to the control plane's `PUT /api/v1/sensors/rig` over its
//!   Unix socket, the on-box caller path the supervisor's own config writes
//!   use. No operator credential is involved or exposed; the plugin's
//!   `sensors.rig.write` grant is the gate, and the route's validation, store
//!   and hub reload are the operator write's own.

use std::collections::VecDeque;
use std::future::Future;
use std::io;

use ados_protocol::media_relay::{
    is_path_segment, parse_whep_location, whep_location, RigWriteReply, RigWriteRequest,
    WhepCloseRequest, WhepOfferReply, WhepOfferRequest, MAX_RIG_BYTES, MAX_SDP_BYTES,
    RIG_UNAVAILABLE, RIG_WRITE_METHOD, WHEP_CLOSE_METHOD, WHEP_OFFER_METHOD, WHEP_UNAVAILABLE,
};
use tokio::io::{AsyncRead, AsyncWrite};

use super::*;

/// The most WHEP locations remembered per plugin. A plugin that opens more
/// without closing them forgets the oldest; the media server still ends those
/// sessions when their peer goes away.
const MAX_SESSIONS_PER_PLUGIN: usize = 32;
/// Both upstreams are on the same box: a connect that does not complete
/// promptly means the service is not running.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// The whole exchange after connecting. The SDK waits longer than this.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(8);
/// The most reply bytes read back from either upstream.
const MAX_REPLY_BYTES: u64 = 1 << 20;
/// The control plane's socket file name under the run dir.
const CONTROL_SOCK_NAME: &str = "control.sock";
/// The rig route on the control plane.
const RIG_ROUTE: &str = "/api/v1/sensors/rig";

/// Where the relays reach, and the WHEP locations each plugin opened.
pub(super) struct MediaRelay {
    whep_port: u16,
    control_sock: PathBuf,
    sessions: Mutex<HashMap<String, VecDeque<String>>>,
}

impl Default for MediaRelay {
    fn default() -> Self {
        Self::new(
            ados_video::mediamtx::DEFAULT_WEBRTC_PORT,
            ados_protocol::paths::run_dir().join(CONTROL_SOCK_NAME),
        )
    }
}

impl MediaRelay {
    pub(super) fn new(whep_port: u16, control_sock: PathBuf) -> Self {
        Self {
            whep_port,
            control_sock,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn remember(&self, plugin_id: &str, location: &str) {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let owned = sessions.entry(plugin_id.to_string()).or_default();
        owned.retain(|l| l != location);
        if owned.len() == MAX_SESSIONS_PER_PLUGIN {
            owned.pop_front();
        }
        owned.push_back(location.to_string());
    }

    fn owns(&self, plugin_id: &str, location: &str) -> bool {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions
            .get(plugin_id)
            .is_some_and(|owned| owned.iter().any(|l| l == location))
    }

    fn forget(&self, plugin_id: &str, location: &str) {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(owned) = sessions.get_mut(plugin_id) {
            owned.retain(|l| l != location);
            if owned.is_empty() {
                sessions.remove(plugin_id);
            }
        }
    }

    /// One exchange with the media server's WHEP endpoint.
    async fn whep(&self, method: &str, path: &str, body: &[u8]) -> io::Result<HttpReply> {
        let port = self.whep_port;
        let stream = connect(tokio::net::TcpStream::connect(("127.0.0.1", port))).await?;
        let host = format!("127.0.0.1:{port}");
        bounded(exchange(
            stream,
            method,
            path,
            &host,
            Some("application/sdp"),
            body,
        ))
        .await
    }

    /// One exchange with the control plane over its Unix socket.
    async fn control(&self, method: &str, path: &str, body: &[u8]) -> io::Result<HttpReply> {
        let stream = connect(tokio::net::UnixStream::connect(&self.control_sock)).await?;
        bounded(exchange(
            stream,
            method,
            path,
            "localhost",
            Some("application/json"),
            body,
        ))
        .await
    }
}

async fn connect<S>(fut: impl Future<Output = io::Result<S>>) -> io::Result<S> {
    tokio::time::timeout(CONNECT_TIMEOUT, fut)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect timed out"))?
}

async fn bounded<T>(fut: impl Future<Output = io::Result<T>>) -> io::Result<T> {
    tokio::time::timeout(EXCHANGE_TIMEOUT, fut)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no answer in time"))?
}

/// What one HTTP exchange answered.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct HttpReply {
    pub(super) status: u16,
    pub(super) location: Option<String>,
    pub(super) body: Vec<u8>,
}

/// Send one `Connection: close` request and read the reply to EOF.
async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    method: &str,
    path: &str,
    host: &str,
    content_type: Option<&str>,
    body: &[u8],
) -> io::Result<HttpReply> {
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(ct) = content_type {
        head.push_str(&format!("Content-Type: {ct}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    let mut raw = Vec::new();
    (&mut stream)
        .take(MAX_REPLY_BYTES)
        .read_to_end(&mut raw)
        .await?;
    parse_reply(&raw)
}

/// Parse a complete HTTP/1.1 reply: status, `Location`, and the body framed by
/// `Content-Length`, chunked encoding, or the close.
pub(super) fn parse_reply(raw: &[u8]) -> io::Result<HttpReply> {
    let split = find(raw, b"\r\n\r\n").ok_or_else(|| invalid("no end of headers"))?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| invalid("headers not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| invalid("no status line"))?;
    let (mut location, mut chunked, mut length) = (None, false, None);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "location" => location = Some(value.to_string()),
            "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
            "content-length" => length = value.parse::<usize>().ok(),
            _ => {}
        }
    }
    let rest = &raw[split + 4..];
    let body = if chunked {
        dechunk(rest)?
    } else if let Some(n) = length {
        rest.get(..n)
            .ok_or_else(|| invalid("body shorter than its length"))?
            .to_vec()
    } else {
        rest.to_vec()
    };
    Ok(HttpReply {
        status,
        location,
        body,
    })
}

fn dechunk(mut rest: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let eol = find(rest, b"\r\n").ok_or_else(|| invalid("chunk size line cut off"))?;
        let line = std::str::from_utf8(&rest[..eol]).map_err(|_| invalid("chunk size"))?;
        let size_hex = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| invalid("chunk size"))?;
        rest = &rest[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        out.extend_from_slice(rest.get(..size).ok_or_else(|| invalid("chunk cut off"))?);
        rest = rest
            .get(size + 2..)
            .ok_or_else(|| invalid("chunk cut off"))?;
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("malformed reply: {what}"),
    )
}

fn decode<T: serde::de::DeserializeOwned>(args: &Value, method: &str) -> Result<T, HostError> {
    rmpv::ext::from_value(args.clone())
        .map_err(|e| HostError::Rpc(format!("{method} arguments are invalid: {e}")))
}

fn whep_unavailable(detail: impl std::fmt::Display) -> HostError {
    HostError::Rpc(format!("{WHEP_UNAVAILABLE}: {detail}"))
}

impl RealHost {
    /// `video.whep.offer`: post the offer for one published leg, remember the
    /// session as `plugin_id`'s, and answer the SDP plus its location.
    pub(super) async fn relay_whep_offer(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let req: WhepOfferRequest = decode(args, WHEP_OFFER_METHOD)?;
        if !is_path_segment(&req.camera_id) {
            return Err(HostError::Rpc(format!(
                "{WHEP_OFFER_METHOD} arguments are invalid: camera_id must match [A-Za-z0-9_-]+"
            )));
        }
        if req.sdp.is_empty() || req.sdp.len() > MAX_SDP_BYTES {
            return Err(HostError::Rpc(format!(
                "{WHEP_OFFER_METHOD} arguments are invalid: sdp must be 1..={MAX_SDP_BYTES} bytes"
            )));
        }
        let camera = req.camera_id;
        let reply = self
            .media_relay
            .whep("POST", &format!("/{camera}/whep"), req.sdp.as_bytes())
            .await
            .map_err(|e| whep_unavailable(format!("media server unreachable: {e}")))?;
        if !(200..300).contains(&reply.status) {
            return Err(whep_unavailable(format!(
                "media server answered {} for camera {camera}",
                reply.status
            )));
        }
        let sdp = String::from_utf8(reply.body)
            .map_err(|_| whep_unavailable("the answer is not UTF-8"))?;
        let prefix = format!("/{camera}/whep/");
        let location = reply
            .location
            .as_deref()
            .and_then(|l| l.strip_prefix(prefix.as_str()))
            .filter(|session| is_path_segment(session))
            .map(|session| whep_location(&camera, session));
        if let Some(loc) = &location {
            self.media_relay.remember(plugin_id, loc);
        }
        serialize_reply(&WhepOfferReply { sdp, location })
    }

    /// `video.whep.close`: tear down a session this plugin opened. A location
    /// the host did not hand this plugin is refused before anything is sent.
    pub(super) async fn relay_whep_close(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let req: WhepCloseRequest = decode(args, WHEP_CLOSE_METHOD)?;
        let parsed = parse_whep_location(&req.location)
            .filter(|_| self.media_relay.owns(plugin_id, &req.location));
        let Some((camera, session)) = parsed else {
            return Err(whep_unavailable(
                "unknown location: not a session this plugin opened",
            ));
        };
        let reply = self
            .media_relay
            .whep("DELETE", &format!("/{camera}/whep/{session}"), &[])
            .await
            .map_err(|e| whep_unavailable(format!("media server unreachable: {e}")))?;
        // Gone already (404) is as closed as a fresh teardown.
        if (200..300).contains(&reply.status) || reply.status == 404 {
            self.media_relay.forget(plugin_id, &req.location);
            return Ok(Value::Map(vec![(
                Value::from("closed"),
                Value::Boolean(true),
            )]));
        }
        Err(whep_unavailable(format!(
            "media server answered {} closing the session",
            reply.status
        )))
    }

    /// `sensors.rig.write`: store the rig through the control plane's rig
    /// route and relay its status and JSON body.
    pub(super) async fn relay_rig_write(&self, args: &Value) -> Result<HostResult, HostError> {
        let req: RigWriteRequest = decode(args, RIG_WRITE_METHOD)?;
        if req.rig_json.len() > MAX_RIG_BYTES {
            return Err(HostError::Rpc(format!(
                "{RIG_WRITE_METHOD} arguments are invalid: rig_json exceeds {MAX_RIG_BYTES} bytes"
            )));
        }
        let reply = self
            .media_relay
            .control("PUT", RIG_ROUTE, req.rig_json.as_bytes())
            .await
            .map_err(|e| HostError::Rpc(format!("{RIG_UNAVAILABLE}: control plane: {e}")))?;
        let body_json = String::from_utf8(reply.body).map_err(|_| {
            HostError::Rpc(format!(
                "{RIG_UNAVAILABLE}: the rig route answered non-UTF-8"
            ))
        })?;
        serialize_reply(&RigWriteReply {
            status: reply.status,
            body_json,
        })
    }
}

#[cfg(test)]
#[path = "media_relay_tests.rs"]
mod tests;
