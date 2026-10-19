//! The live-video and rig relay plugin methods: `video.whep.offer`,
//! `video.whep.close` and `sensors.rig.write`.
//!
//! A sandboxed plugin cannot reach the loopback media server or the control
//! plane's socket. The host can, so it relays on the plugin's behalf:
//!
//! * `video.whep.offer` posts a viewer's SDP offer to the media server's WHEP
//!   endpoint for one configured camera and returns the answer plus an opaque
//!   `location` naming the session. The host resolves the camera id to the
//!   media leg serving it from the node's own camera roster; the plugin never
//!   names a media path. The location is the agent's own WHEP form
//!   (`/whep/<session>?camera=<leg>`), never a host address.
//! * `video.whep.close` tears down a session, and only one whose location the
//!   host returned to the same plugin.
//! * `sensors.rig.write` stores a rig document through the control plane's
//!   `PUT /api/v1/sensors/rig`, so it gets the operator route's validation and
//!   hub reload; the reply carries that route's HTTP status and JSON body.
//!
//! The types are the wire shape: the host decodes the request and serializes
//! the reply with them, and the SDK does the reverse, so the two cannot drift.

use serde::{Deserialize, Serialize};

/// The capability both WHEP methods are gated on.
pub const WHEP_CAPABILITY: &str = "video.whep.relay";
/// Open a playback session.
pub const WHEP_OFFER_METHOD: &str = "video.whep.offer";
/// Close a playback session this plugin opened.
pub const WHEP_CLOSE_METHOD: &str = "video.whep.close";
/// The capability `sensors.rig.write` is gated on.
pub const RIG_CAPABILITY: &str = "sensors.rig.write";
/// Store a rig document.
pub const RIG_WRITE_METHOD: &str = "sensors.rig.write";

/// Error prefix when the media server cannot serve the exchange (not running,
/// the offer refused, an unknown location).
pub const WHEP_UNAVAILABLE: &str = "whep_unavailable";
/// Error prefix when `video.whep.offer` names anything that is not a
/// configured, enabled camera of this node.
pub const UNKNOWN_CAMERA: &str = "unknown_camera";
/// Error prefix when the control plane's rig route cannot be reached.
pub const RIG_UNAVAILABLE: &str = "rig_unavailable";

/// The largest SDP offer relayed, matching the agent's own WHEP front.
pub const MAX_SDP_BYTES: usize = 64 * 1024;
/// The largest rig document relayed.
pub const MAX_RIG_BYTES: usize = 256 * 1024;

/// `video.whep.offer` arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhepOfferRequest {
    /// A configured, enabled camera's roster id. The host serves it from the
    /// leg that publishes it (`main` for the primary camera).
    pub camera_id: String,
    /// The viewer's SDP offer.
    pub sdp: String,
}

/// `video.whep.offer` reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhepOfferReply {
    /// The SDP answer.
    pub sdp: String,
    /// The opaque session location `video.whep.close` takes; `None` when the
    /// media server named none (the session then ends with the peer).
    pub location: Option<String>,
}

/// `video.whep.close` arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhepCloseRequest {
    /// A location a previous `video.whep.offer` returned to this plugin.
    pub location: String,
}

/// `sensors.rig.write` arguments. The document travels as JSON text so the
/// control plane validates exactly what the plugin wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RigWriteRequest {
    pub rig_json: String,
}

/// `sensors.rig.write` reply: the control plane's HTTP status and JSON body
/// (`{"ok":true,"rig_id","calib_version"}` or `{"error":"E_RIG_INVALID",
/// "detail"}`), the body as JSON text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RigWriteReply {
    pub status: u16,
    pub body_json: String,
}

/// `[A-Za-z0-9_-]+`: the roster's leg-id rule and what a media-server session
/// id is made of. Anything else could address another media-server route.
pub fn is_path_segment(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// The location handed to the plugin for session `session` on leg `camera`.
pub fn whep_location(camera: &str, session: &str) -> String {
    format!("/whep/{session}?camera={camera}")
}

/// Parse a location [`whep_location`] built back into `(camera, session)`.
/// Anything else, including a segment outside [`is_path_segment`], is `None`.
pub fn parse_whep_location(location: &str) -> Option<(String, String)> {
    let rest = location.strip_prefix("/whep/")?;
    let (session, query) = rest.split_once("?camera=")?;
    (is_path_segment(session) && is_path_segment(query))
        .then(|| (query.to_string(), session.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_location_round_trips() {
        let loc = whep_location("main", "a1-b_2");
        assert_eq!(loc, "/whep/a1-b_2?camera=main");
        assert_eq!(
            parse_whep_location(&loc),
            Some(("main".into(), "a1-b_2".into()))
        );
    }

    #[test]
    fn a_foreign_location_is_refused() {
        for bad in [
            "/whep/s",
            "/main/whep/s",
            "http://127.0.0.1:8889/main/whep/s",
            "/whep/../x?camera=main",
            "/whep/s?camera=main&x=1",
            "/whep/s?camera=",
            "/whep/?camera=main",
        ] {
            assert_eq!(parse_whep_location(bad), None, "{bad}");
        }
    }
}
