//! Classification and payload extraction for the engine's push envelopes.

use ados_protocol::framebus::methods;
use rmpv::{Value, ValueRef};

/// Which of the engine's two push events an envelope body is, if either.
pub(super) enum PushKind {
    Frame,
    Detection,
    Other,
}

/// Classify an envelope body by its `method` field without decoding it.
///
/// `rmpv`'s borrowing reader walks the bytes and copies no payload, so a push
/// the host is about to drop costs one structural walk instead of a full
/// [`Envelope`](ados_protocol::plugin::Envelope) — which allocates the binary
/// `descriptor` once in `args` and again in the extraction below. Anything that
/// is not recognisably one of the two push methods is [`PushKind::Other`] and
/// takes the normal decode path, so a response is never misrouted by a partial
/// read.
pub(super) fn peek_push_kind(body: &[u8]) -> PushKind {
    let Ok(ValueRef::Map(entries)) = rmpv::decode::read_value_ref(&mut &body[..]) else {
        return PushKind::Other;
    };
    for (key, value) in entries {
        let ValueRef::String(key) = key else { continue };
        if key.as_str() != Some("method") {
            continue;
        }
        let ValueRef::String(method) = value else {
            return PushKind::Other;
        };
        return match method.as_str() {
            Some(methods::DELIVER_FRAME) => PushKind::Frame,
            Some(methods::DELIVER_DETECTION) => PushKind::Detection,
            _ => PushKind::Other,
        };
    }
    PushKind::Other
}

/// Extract the frame-descriptor bytes from a `vision.deliver` envelope. The
/// engine carries the encoded [`ados_protocol::framebus::FrameDescriptor`] as a
/// binary `descriptor` field; the host forwards those bytes unchanged to the
/// plugin. Returns `None` if the field is absent or not binary.
pub(super) fn frame_descriptor_bytes(args: &Value) -> Option<Vec<u8>> {
    match args {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some("descriptor"))
            .and_then(|(_, v)| match v {
                Value::Binary(b) => Some(b.clone()),
                _ => None,
            }),
        _ => None,
    }
}

/// Extract the detection-batch bytes from a `vision.deliver_detection` envelope.
/// The engine carries the encoded [`ados_protocol::framebus::DetectionBatch`] as
/// a binary `batch` field; the host forwards those bytes unchanged to the
/// plugin. Returns `None` if the field is absent or not binary.
pub(super) fn detection_batch_bytes(args: &Value) -> Option<Vec<u8>> {
    match args {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some("batch"))
            .and_then(|(_, v)| match v {
                Value::Binary(b) => Some(b.clone()),
                _ => None,
            }),
        _ => None,
    }
}
