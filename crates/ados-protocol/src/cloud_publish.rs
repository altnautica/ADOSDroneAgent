//! The cloud-publish socket: how a plugin's agent half sends data off the node
//! through the cloud relay.
//!
//! The plugin host holds the capability gate (`cloud.publish`, `cloud.records`)
//! and forwards an admitted call here; `ados-cloud`, which owns the broker
//! session and the device's cloud credentials, serves the socket and does the
//! actual publish. A plugin never reaches this socket itself: it is root-only
//! (`0o600`) and hidden from the plugin sandbox with the rest of the run dir.
//!
//! ## Wire
//!
//! A request is one [`crate::frame`] frame (4-byte big-endian length, then the
//! body; zero-length rejected, at most [`MAX_FRAME`] bytes) whose body is a
//! msgpack map with string keys:
//!
//! ```text
//! { v: 1, kind: "stream" | "record", plugin_id: str,
//!   stream?: str, collection?: str, key?: str, device_id?: str,
//!   payload: bin }
//! ```
//!
//! The reply is one frame of the same framing, a msgpack map
//! `{ ok: bool, error?: str }`. A connection may carry several request/reply
//! exchanges in sequence; [`send`] uses one connection per request.
//!
//! * `kind: "stream"` carries `stream` and no `collection`/`key`/`device_id`.
//!   The relay publishes the payload at QoS 0 on
//!   `ados/{device_id}/plugin/{plugin_id}/{stream}` under the node's own device
//!   id. The one exception is [`VISION_DETECTIONS_STREAM`]: its payload must be
//!   a JSON `vision.detection` batch, and it goes to the core topic
//!   `ados/{device_id}/vision/detections` the ground station already reads.
//!   `ok` means the message entered the relay's bounded publish queue. The lane
//!   is lossy by design, so it does not mean the broker received it.
//! * `kind: "record"` carries `collection` and `key` (and optionally a subject
//!   `device_id`, defaulting to this node). The payload is the record's JSON.
//!   The relay upserts it into the account's plugin records, and `ok` is the
//!   cloud's own answer to that write.
//!
//! Names are constrained so they are safe as topic segments and as record
//! identifiers: see [`validate_stream_name`], [`validate_collection`],
//! [`validate_record_key`] and [`validate_plugin_id`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

use crate::frame::{encode_frame, FrameError};

/// The contract's wire version, stamped on every request and checked on decode.
/// Mirrors the `cloud-publish` entry in the contract registry.
pub const CLOUD_PUBLISH_VERSION: u16 = 1;

/// Socket file name under the agent run directory.
pub const CLOUD_PUBLISH_SOCK_NAME: &str = "cloud-publish.sock";

/// Default agent run directory, overridden by `ADOS_RUN_DIR`.
const DEFAULT_RUN_DIR: &str = "/run/ados";

/// The socket's file mode: owner (root) only. Only the plugin host, which runs
/// as root and has already applied the capability gate, may publish.
pub const SOCKET_MODE: u32 = 0o600;

/// Largest frame body either side accepts.
pub const MAX_FRAME: usize = 80 * 1024;

/// Largest payload a request may carry.
pub const MAX_PAYLOAD: usize = 64 * 1024;

/// Longest record key, in UTF-8 bytes.
pub const MAX_RECORD_KEY_BYTES: usize = 256;

/// Longest plugin id.
pub const MAX_PLUGIN_ID_LEN: usize = 128;

/// Longest subject device id on a record.
pub const MAX_DEVICE_ID_LEN: usize = 64;

/// The stream name that maps to the core vision-detection topic instead of the
/// plugin's own subtree. The plugin host admits it only for a plugin that also
/// holds `vision.detection.publish`.
pub const VISION_DETECTIONS_STREAM: &str = "vision.detections";

/// Bound on a stream exchange. The relay answers as soon as the message is
/// queued, so anything slower is a stall.
pub const STREAM_REPLY_TIMEOUT: Duration = Duration::from_secs(2);

/// Bound on a record exchange. The relay answers only once the cloud has
/// answered the write, and its own HTTP client gives up after 10 s.
pub const RECORD_REPLY_TIMEOUT: Duration = Duration::from_secs(15);

/// The socket path, honouring the `ADOS_RUN_DIR` override so a test or a
/// development host does not collide with the running service.
pub fn socket_path() -> PathBuf {
    match std::env::var("ADOS_RUN_DIR") {
        Ok(dir) if !dir.trim().is_empty() => Path::new(&dir).join(CLOUD_PUBLISH_SOCK_NAME),
        _ => Path::new(DEFAULT_RUN_DIR).join(CLOUD_PUBLISH_SOCK_NAME),
    }
}

/// What a request asks the relay to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CloudPublishKind {
    /// A lossy live message on the broker (QoS 0).
    Stream,
    /// A keyed JSON record upserted into the account's plugin records.
    Record,
}

impl CloudPublishKind {
    /// How long [`send`] waits for the relay's reply to this kind of request.
    pub fn reply_timeout(self) -> Duration {
        match self {
            Self::Stream => STREAM_REPLY_TIMEOUT,
            Self::Record => RECORD_REPLY_TIMEOUT,
        }
    }
}

/// One request on the socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudPublishRequest {
    pub v: u16,
    pub kind: CloudPublishKind,
    /// The publishing plugin, bound by the plugin host from the caller's
    /// identity, never taken from plugin input.
    pub plugin_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The record's subject device; `None` means this node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(with = "bin")]
    pub payload: Vec<u8>,
}

impl CloudPublishRequest {
    /// A stream request.
    pub fn stream(
        plugin_id: impl Into<String>,
        stream: impl Into<String>,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            v: CLOUD_PUBLISH_VERSION,
            kind: CloudPublishKind::Stream,
            plugin_id: plugin_id.into(),
            stream: Some(stream.into()),
            collection: None,
            key: None,
            device_id: None,
            payload,
        }
    }

    /// A record request. `payload` is the record's JSON encoding.
    pub fn record(
        plugin_id: impl Into<String>,
        collection: impl Into<String>,
        key: impl Into<String>,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            v: CLOUD_PUBLISH_VERSION,
            kind: CloudPublishKind::Record,
            plugin_id: plugin_id.into(),
            stream: None,
            collection: Some(collection.into()),
            key: Some(key.into()),
            device_id: None,
            payload,
        }
    }

    /// Name the record's subject device (a record only).
    pub fn with_device_id(mut self, device_id: impl Into<String>) -> Self {
        self.device_id = Some(device_id.into());
        self
    }

    /// Check the request against the contract: version, plugin id, the fields
    /// its kind requires and forbids, every name's grammar, and the payload cap.
    pub fn validate(&self) -> Result<(), CloudPublishError> {
        if self.v != CLOUD_PUBLISH_VERSION {
            return Err(CloudPublishError::Invalid(format!(
                "unsupported cloud-publish version {} (this build speaks {CLOUD_PUBLISH_VERSION})",
                self.v
            )));
        }
        validate_plugin_id(&self.plugin_id)?;
        if self.payload.len() > MAX_PAYLOAD {
            return Err(CloudPublishError::Invalid(format!(
                "payload of {} bytes exceeds {MAX_PAYLOAD}",
                self.payload.len()
            )));
        }
        match self.kind {
            CloudPublishKind::Stream => {
                let stream = self
                    .stream
                    .as_deref()
                    .ok_or_else(|| invalid("a stream request needs a stream name"))?;
                validate_stream_name(stream)?;
                if self.collection.is_some() || self.key.is_some() || self.device_id.is_some() {
                    return Err(invalid(
                        "a stream request carries no collection, key or device_id",
                    ));
                }
            }
            CloudPublishKind::Record => {
                let collection = self
                    .collection
                    .as_deref()
                    .ok_or_else(|| invalid("a record request needs a collection"))?;
                validate_collection(collection)?;
                let key = self
                    .key
                    .as_deref()
                    .ok_or_else(|| invalid("a record request needs a key"))?;
                validate_record_key(key)?;
                if let Some(device_id) = self.device_id.as_deref() {
                    validate_device_id(device_id)?;
                }
                if self.stream.is_some() {
                    return Err(invalid("a record request carries no stream name"));
                }
            }
        }
        Ok(())
    }

    /// Validate, then encode as one length-prefixed frame.
    pub fn encode(&self) -> Result<Vec<u8>, CloudPublishError> {
        self.validate()?;
        let body =
            rmp_serde::to_vec_named(self).map_err(|e| CloudPublishError::Encode(e.to_string()))?;
        frame_body(&body)
    }

    /// Decode a frame body (length prefix already consumed) and validate it.
    pub fn decode(body: &[u8]) -> Result<Self, CloudPublishError> {
        let req: Self =
            rmp_serde::from_slice(body).map_err(|e| CloudPublishError::Decode(e.to_string()))?;
        req.validate()?;
        Ok(req)
    }
}

/// The relay's answer to one request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudPublishReply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl CloudPublishReply {
    /// The request was accepted.
    pub fn accepted() -> Self {
        Self {
            ok: true,
            error: None,
        }
    }

    /// The request was refused, with the reason.
    pub fn refused(reason: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(reason.into()),
        }
    }

    /// Encode as one length-prefixed frame.
    pub fn encode(&self) -> Result<Vec<u8>, CloudPublishError> {
        let body =
            rmp_serde::to_vec_named(self).map_err(|e| CloudPublishError::Encode(e.to_string()))?;
        frame_body(&body)
    }

    /// Decode a frame body (length prefix already consumed).
    pub fn decode(body: &[u8]) -> Result<Self, CloudPublishError> {
        rmp_serde::from_slice(body).map_err(|e| CloudPublishError::Decode(e.to_string()))
    }
}

/// Why a request did not get a reply, or was refused before it was sent.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CloudPublishError {
    /// The request breaks the contract; nothing was sent.
    #[error("invalid cloud-publish request: {0}")]
    Invalid(String),
    #[error("cloud-publish encode failed: {0}")]
    Encode(String),
    #[error("cloud-publish decode failed: {0}")]
    Decode(String),
    /// The socket could not be reached, or the exchange failed mid-way.
    #[error("cloud-publish socket: {0}")]
    Io(String),
    #[error("cloud-publish reply timed out")]
    Timeout,
}

fn invalid(msg: &str) -> CloudPublishError {
    CloudPublishError::Invalid(msg.to_string())
}

fn frame_body(body: &[u8]) -> Result<Vec<u8>, CloudPublishError> {
    encode_frame(body, MAX_FRAME).map_err(|e| match e {
        FrameError::TooLarge { len, max } => {
            CloudPublishError::Encode(format!("frame of {len} bytes exceeds {max}"))
        }
        other => CloudPublishError::Encode(other.to_string()),
    })
}

/// Send one request and wait for the relay's reply, bounded by the kind's
/// [`reply_timeout`](CloudPublishKind::reply_timeout). An invalid request is
/// refused before anything is connected. `Ok` carries the relay's answer,
/// which may itself be a refusal (`ok: false`).
pub async fn send(
    path: &Path,
    req: &CloudPublishRequest,
) -> Result<CloudPublishReply, CloudPublishError> {
    let frame = req.encode()?;
    tokio::time::timeout(req.kind.reply_timeout(), exchange(path, &frame))
        .await
        .map_err(|_| CloudPublishError::Timeout)?
}

async fn exchange(path: &Path, frame: &[u8]) -> Result<CloudPublishReply, CloudPublishError> {
    let io = |e: std::io::Error| CloudPublishError::Io(e.to_string());
    let mut stream = UnixStream::connect(path).await.map_err(io)?;
    stream.write_all(frame).await.map_err(io)?;
    let body = crate::ipc::read_length_prefixed(&mut stream, MAX_FRAME, true)
        .await
        .map_err(io)?
        .ok_or_else(|| CloudPublishError::Io("relay closed the connection".into()))?;
    CloudPublishReply::decode(&body)
}

/// A plugin id: reverse-DNS lowercase (`^[a-z0-9]+(\.[a-z0-9-]+)+$`, the
/// manifest rule), at most [`MAX_PLUGIN_ID_LEN`] characters. It becomes a topic
/// segment, so it can never carry `/`, `+` or `#`.
pub fn validate_plugin_id(id: &str) -> Result<(), CloudPublishError> {
    let ok = id.len() <= MAX_PLUGIN_ID_LEN
        && id.split_once('.').is_some_and(|(head, tail)| {
            !head.is_empty()
                && head.bytes().all(is_lower_alnum)
                && tail
                    .split('.')
                    .all(|s| !s.is_empty() && s.bytes().all(|b| is_lower_alnum(b) || b == b'-'))
        });
    if ok {
        Ok(())
    } else {
        Err(CloudPublishError::Invalid(format!(
            "plugin id {id:?} must be reverse-DNS lowercase, at most {MAX_PLUGIN_ID_LEN} characters"
        )))
    }
}

/// A stream name: `[a-z0-9][a-z0-9._-]{0,63}`.
pub fn validate_stream_name(stream: &str) -> Result<(), CloudPublishError> {
    let bytes = stream.as_bytes();
    let ok = (1..=64).contains(&bytes.len())
        && is_lower_alnum(bytes[0])
        && bytes[1..]
            .iter()
            .all(|&b| is_lower_alnum(b) || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(CloudPublishError::Invalid(format!(
            "stream name {stream:?} must match [a-z0-9][a-z0-9._-]{{0,63}}"
        )))
    }
}

/// A record collection: `[a-z0-9_.-]{1,64}`.
pub fn validate_collection(collection: &str) -> Result<(), CloudPublishError> {
    let ok = (1..=64).contains(&collection.len())
        && collection
            .bytes()
            .all(|b| is_lower_alnum(b) || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(CloudPublishError::Invalid(format!(
            "collection {collection:?} must match [a-z0-9_.-]{{1,64}}"
        )))
    }
}

/// A record key: non-empty, at most [`MAX_RECORD_KEY_BYTES`] UTF-8 bytes.
pub fn validate_record_key(key: &str) -> Result<(), CloudPublishError> {
    if !key.is_empty() && key.len() <= MAX_RECORD_KEY_BYTES {
        Ok(())
    } else {
        Err(CloudPublishError::Invalid(format!(
            "record key must be 1-{MAX_RECORD_KEY_BYTES} bytes, got {}",
            key.len()
        )))
    }
}

/// A record's subject device id: `[A-Za-z0-9._-]{1,64}`.
pub fn validate_device_id(device_id: &str) -> Result<(), CloudPublishError> {
    let ok = (1..=MAX_DEVICE_ID_LEN).contains(&device_id.len())
        && device_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(CloudPublishError::Invalid(format!(
            "device id {device_id:?} must match [A-Za-z0-9._-]{{1,{MAX_DEVICE_ID_LEN}}}"
        )))
    }
}

fn is_lower_alnum(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit()
}

/// The payload travels as msgpack `bin`, not as an array of integers.
mod bin {
    use serde::{de, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(bytes)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        struct BinVisitor;

        impl<'de> de::Visitor<'de> for BinVisitor {
            type Value = Vec<u8>;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a binary payload")
            }

            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Vec<u8>, E> {
                Ok(v.to_vec())
            }

            fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
                Ok(v)
            }
        }

        d.deserialize_byte_buf(BinVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;

    fn body_of(frame: &[u8]) -> &[u8] {
        let len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(frame.len(), 4 + len, "one whole frame");
        &frame[4..]
    }

    #[test]
    fn version_matches_the_contract_registry() {
        assert_eq!(
            Some(CLOUD_PUBLISH_VERSION),
            crate::contracts::contract_version("cloud-publish")
        );
    }

    #[test]
    fn a_stream_request_round_trips_with_a_binary_payload() {
        let req = CloudPublishRequest::stream("com.example.mapper", "pose", vec![0, 1, 2, 255]);
        let frame = req.encode().unwrap();
        let body = body_of(&frame);

        // The payload is msgpack bin and the map keys are the contract's names,
        // so a reader in any language decodes it without this crate.
        let raw: rmpv::Value = rmp_serde::from_slice(body).unwrap();
        let map: Vec<(String, rmpv::Value)> = raw
            .as_map()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.as_str().unwrap().to_string(), v.clone()))
            .collect();
        let get = |k: &str| map.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        assert_eq!(get("v"), Some(rmpv::Value::from(1)));
        assert_eq!(get("kind"), Some(rmpv::Value::from("stream")));
        assert_eq!(
            get("plugin_id"),
            Some(rmpv::Value::from("com.example.mapper"))
        );
        assert_eq!(get("stream"), Some(rmpv::Value::from("pose")));
        assert_eq!(
            get("payload"),
            Some(rmpv::Value::Binary(vec![0, 1, 2, 255]))
        );
        assert_eq!(get("collection"), None, "absent optionals are not sent");

        assert_eq!(CloudPublishRequest::decode(body).unwrap(), req);
    }

    #[test]
    fn a_record_request_round_trips_with_its_subject_device() {
        let req = CloudPublishRequest::record(
            "com.example.mapper",
            "jobs",
            "session-7",
            br#"{"status":"done"}"#.to_vec(),
        )
        .with_device_id("a1b2c3d4e5f6");
        let frame = req.encode().unwrap();
        assert_eq!(CloudPublishRequest::decode(body_of(&frame)).unwrap(), req);
    }

    #[test]
    fn a_reply_round_trips_in_both_shapes() {
        for reply in [
            CloudPublishReply::accepted(),
            CloudPublishReply::refused("cloud relay is not paired"),
        ] {
            let frame = reply.encode().unwrap();
            assert_eq!(CloudPublishReply::decode(body_of(&frame)).unwrap(), reply);
        }
        // An accepted reply carries no error key at all.
        let raw: rmpv::Value =
            rmp_serde::from_slice(body_of(&CloudPublishReply::accepted().encode().unwrap()))
                .unwrap();
        assert_eq!(raw.as_map().unwrap().len(), 1);
    }

    #[test]
    fn a_payload_over_the_cap_is_refused_and_one_at_the_cap_is_not() {
        let at_cap = CloudPublishRequest::stream("com.example.a", "s", vec![7; MAX_PAYLOAD]);
        let frame = at_cap
            .encode()
            .expect("a payload at the cap fits the frame");
        assert!(frame.len() - 4 <= MAX_FRAME);

        let over = CloudPublishRequest::stream("com.example.a", "s", vec![7; MAX_PAYLOAD + 1]);
        assert!(matches!(over.encode(), Err(CloudPublishError::Invalid(_))));
    }

    #[test]
    fn decode_refuses_an_oversize_payload_even_from_a_hand_built_frame() {
        let mut over = CloudPublishRequest::stream("com.example.a", "s", Vec::new());
        over.payload = vec![0; MAX_PAYLOAD + 1];
        let body = rmp_serde::to_vec_named(&over).unwrap();
        assert!(matches!(
            CloudPublishRequest::decode(&body),
            Err(CloudPublishError::Invalid(_))
        ));
    }

    #[test]
    fn decode_refuses_another_version() {
        let mut req = CloudPublishRequest::stream("com.example.a", "s", Vec::new());
        req.v = CLOUD_PUBLISH_VERSION + 1;
        let body = rmp_serde::to_vec_named(&req).unwrap();
        assert!(matches!(
            CloudPublishRequest::decode(&body),
            Err(CloudPublishError::Invalid(_))
        ));
    }

    #[test]
    fn a_kind_must_carry_exactly_its_own_fields() {
        let mut stream_without_name = CloudPublishRequest::stream("com.example.a", "s", vec![]);
        stream_without_name.stream = None;
        let mut stream_with_key = CloudPublishRequest::stream("com.example.a", "s", vec![]);
        stream_with_key.key = Some("k".into());
        let stream_with_device =
            CloudPublishRequest::stream("com.example.a", "s", vec![]).with_device_id("dev1");
        let mut record_without_key = CloudPublishRequest::record("com.example.a", "c", "k", vec![]);
        record_without_key.key = None;
        let mut record_with_stream = CloudPublishRequest::record("com.example.a", "c", "k", vec![]);
        record_with_stream.stream = Some("s".into());

        for req in [
            stream_without_name,
            stream_with_key,
            stream_with_device,
            record_without_key,
            record_with_stream,
        ] {
            assert!(req.validate().is_err(), "{req:?}");
        }
    }

    #[test]
    fn stream_names_follow_the_grammar() {
        for good in ["a", "0", "pose", "map.keyframe", "a_b-c.d", &"a".repeat(64)] {
            assert!(validate_stream_name(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            ".pose",
            "-pose",
            "_pose",
            "Pose",
            "a/b",
            "a+b",
            "a#",
            "a b",
            &"a".repeat(65),
        ] {
            assert!(validate_stream_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn collections_follow_the_grammar() {
        for good in ["jobs", "a", ".x", "_x", "a.b_c-d", &"z".repeat(64)] {
            assert!(validate_collection(good).is_ok(), "{good}");
        }
        for bad in ["", "Jobs", "a/b", "a b", &"z".repeat(65)] {
            assert!(validate_collection(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn record_keys_are_capped_in_bytes_not_characters() {
        assert!(validate_record_key("k").is_ok());
        assert!(validate_record_key(&"k".repeat(MAX_RECORD_KEY_BYTES)).is_ok());
        assert!(validate_record_key("").is_err());
        assert!(validate_record_key(&"k".repeat(MAX_RECORD_KEY_BYTES + 1)).is_err());
        // 86 three-byte characters are 258 bytes: over the cap at 86 characters.
        assert!(validate_record_key(&"€".repeat(86)).is_err());
        assert!(validate_record_key(&"€".repeat(85)).is_ok());
    }

    #[test]
    fn plugin_ids_are_reverse_dns_and_topic_safe() {
        for good in ["com.example.a", "a.b", "com.example.multi-word", "x9.y-z.q"] {
            assert!(validate_plugin_id(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "single",
            "com.",
            ".com.x",
            "com..x",
            "Com.example.a",
            "com-x.example",
            "com.example/a",
            "com.example.+",
            "com.example.#",
        ] {
            assert!(validate_plugin_id(bad).is_err(), "{bad}");
        }
        let long = format!("com.{}", "a".repeat(MAX_PLUGIN_ID_LEN));
        assert!(validate_plugin_id(&long).is_err());
    }

    #[test]
    fn subject_device_ids_are_bounded_tokens() {
        assert!(validate_device_id("a1b2c3d4e5f6").is_ok());
        for bad in ["", "a/b", "a b", &"d".repeat(MAX_DEVICE_ID_LEN + 1)] {
            assert!(validate_device_id(bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn send_writes_one_frame_and_returns_the_relays_reply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CLOUD_PUBLISH_SOCK_NAME);
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let body = crate::ipc::read_length_prefixed(&mut s, MAX_FRAME, true)
                .await
                .unwrap()
                .unwrap();
            let req = CloudPublishRequest::decode(&body).unwrap();
            let reply = CloudPublishReply::refused(format!("saw {}", req.plugin_id));
            s.write_all(&reply.encode().unwrap()).await.unwrap();
            // The client reads exactly one reply frame; nothing else follows.
            let mut rest = Vec::new();
            s.read_to_end(&mut rest).await.unwrap();
            assert!(rest.is_empty());
        });

        let reply = send(
            &path,
            &CloudPublishRequest::stream("com.example.a", "s", b"x".to_vec()),
        )
        .await
        .unwrap();
        assert_eq!(reply, CloudPublishReply::refused("saw com.example.a"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn send_refuses_an_invalid_request_without_connecting() {
        // No socket exists at this path, so an attempted connect would surface
        // as Io rather than Invalid.
        let dir = tempfile::tempdir().unwrap();
        let req = CloudPublishRequest::stream("com.example.a", "Bad Name", vec![]);
        assert!(matches!(
            send(&dir.path().join("absent.sock"), &req).await,
            Err(CloudPublishError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn send_reports_an_absent_socket_as_io() {
        let dir = tempfile::tempdir().unwrap();
        let req = CloudPublishRequest::stream("com.example.a", "s", vec![]);
        assert!(matches!(
            send(&dir.path().join("absent.sock"), &req).await,
            Err(CloudPublishError::Io(_))
        ));
    }
}
