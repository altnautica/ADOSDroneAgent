//! `pilot.v1` messages on the ground station's relay socket (`/run/ados/pilot.sock`).
//! The framing is `ados_protocol::pilot`'s; this module builds and reads the
//! message bodies.

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use ados_protocol::pilot::PilotWireError;

/// The `src` this daemon writes.
pub const SRC: &str = "gs-native";

/// The msgpack body of a message.
pub fn encode(msg: &Value) -> Result<Vec<u8>, PilotWireError> {
    rmp_serde::to_vec_named(msg).map_err(|e| PilotWireError::Encode(e.to_string()))
}

pub async fn write_msg<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    msg: &Value,
) -> Result<(), PilotWireError> {
    ados_protocol::pilot::write_msg(w, &encode(msg)?).await
}

/// The next message decoded as JSON, `None` on a clean EOF. Not cancel-safe:
/// run it in a task of its own, never as a `select!` branch.
pub async fn read_msg<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<Option<Value>, PilotWireError> {
    let Some(body) = ados_protocol::pilot::read_msg(r).await? else {
        return Ok(None);
    };
    rmp_serde::from_slice(&body)
        .map(Some)
        .map_err(|e| PilotWireError::Malformed {
            op: "?".into(),
            err: e.to_string(),
        })
}

pub fn input(session: u32, seq: u32, t_ms: u64, axes: [f32; 4], buttons: u32) -> Value {
    json!({"op":"input","src":SRC,"session":session,"seq":seq,"t_ms":t_ms,"axes":axes,"buttons":buttons})
}

pub fn release(session: u32) -> Value {
    json!({"op":"release","src":SRC,"session":session})
}

/// A binding action onto the pilot `action` name. The spec's `rth_cancel`
/// binding (PS during RTH) cancels to a hold, which is the pilot's `pause`;
/// the pilot's own `rth_cancel` only cancels a low-battery countdown.
pub fn action(binding_action: &str) -> Value {
    let name = if binding_action == "rth_cancel" {
        "pause"
    } else {
        binding_action
    };
    json!({"op":"action","name":name,"src":SRC})
}

pub fn subscribe() -> Value {
    json!({"op":"subscribe"})
}

/// A fresh, non-zero input session id.
pub fn new_session() -> u32 {
    loop {
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let s = u32::from_le_bytes(b);
        if s != 0 {
            return s;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip_and_decode_as_pilot_requests() {
        let msg = input(7, 3, 1234, [0.5, -1.0, 0.0, 0.25], 0b101);
        let mut buf = Vec::new();
        write_msg(&mut buf, &msg).await.unwrap();
        write_msg(&mut buf, &action("rth_cancel")).await.unwrap();
        let mut r = &buf[..];
        let back = read_msg(&mut r).await.unwrap().unwrap();
        assert_eq!(back["op"], "input");
        assert_eq!(back["src"], "gs-native");
        assert_eq!(back["axes"][1], -1.0);
        assert_eq!(back["buttons"], 5);
        // The pilot's own decoder accepts the bodies.
        let body = encode(&msg).unwrap();
        ados_protocol::pilot::decode_request(&body).unwrap();
        ados_protocol::pilot::decode_request(&encode(&action("rth_cancel")).unwrap()).unwrap();
        ados_protocol::pilot::decode_request(&encode(&release(7)).unwrap()).unwrap();
        assert_eq!(read_msg(&mut r).await.unwrap().unwrap()["name"], "pause");
        assert!(read_msg(&mut r).await.unwrap().is_none());
    }

    #[test]
    fn sessions_are_non_zero() {
        assert!((0..100).all(|_| new_session() != 0));
    }
}
