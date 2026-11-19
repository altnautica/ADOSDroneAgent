//! The uplink ops of the command socket: `uplink_take`, `uplink_give_back`,
//! `uplink_take_back` and `uplink_status`. The LAN route authenticates the
//! caller (an approved phone, or the box's own operator) and forwards here;
//! this side applies the transition against the process's uplink.

use serde::Deserialize;
use serde_json::Value;

use crate::uplink::ops::{self, TakeContext, TakeRequest};
use crate::uplink::uplink;

#[derive(Debug, Deserialize)]
struct UplinkRequest {
    op: String,
    #[serde(default)]
    holder_device_id: Option<String>,
    #[serde(default)]
    holder_name: Option<String>,
    #[serde(default)]
    phone_key: Option<String>,
    #[serde(default)]
    yield_url: Option<String>,
    #[serde(default)]
    phone_confirmed: bool,
}

/// A parsed uplink op.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum UplinkCommand {
    Take(TakeRequest),
    GiveBack {
        holder_device_id: String,
        phone_key: String,
    },
    TakeBack {
        phone_confirmed: bool,
    },
    Status,
}

/// `None` for a line that is not an uplink op (the other ops' parser takes
/// it); otherwise the command or its terminal reply.
pub(super) fn parse(line: &[u8]) -> Option<Result<UplinkCommand, Value>> {
    let req: UplinkRequest = serde_json::from_slice(line).ok()?;
    let missing = |field: &str| {
        Err(ops::refusal(
            "E_BAD_REQUEST",
            &format!("The request is missing {field}."),
        ))
    };
    let parsed = match req.op.as_str() {
        "uplink_take" => {
            let (Some(holder_device_id), Some(phone_key)) = (
                req.holder_device_id,
                req.phone_key.filter(|k| !k.is_empty()),
            ) else {
                return Some(missing("holder_device_id or phone_key"));
            };
            let holder_name = req
                .holder_name
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| holder_device_id.clone());
            Ok(UplinkCommand::Take(TakeRequest {
                holder_device_id,
                holder_name,
                phone_key,
                yield_url: req.yield_url.filter(|u| !u.is_empty()),
            }))
        }
        "uplink_give_back" => match (req.holder_device_id, req.phone_key) {
            (Some(holder_device_id), Some(phone_key)) => Ok(UplinkCommand::GiveBack {
                holder_device_id,
                phone_key,
            }),
            _ => missing("holder_device_id or phone_key"),
        },
        "uplink_take_back" => Ok(UplinkCommand::TakeBack {
            phone_confirmed: req.phone_confirmed,
        }),
        "uplink_status" => Ok(UplinkCommand::Status),
        _ => return None,
    };
    Some(parsed)
}

/// Serve one line if it is an uplink op.
pub(super) async fn dispatch(line: &[u8], direct_role: bool) -> Option<Value> {
    let cmd = match parse(line)? {
        Ok(cmd) => cmd,
        Err(reply) => return Some(reply),
    };
    let u = uplink();
    Some(match cmd {
        UplinkCommand::Take(req) => {
            let ctx = TakeContext {
                bind_active: ados_supervisor::bind::orchestrator::read_sentinel_active(),
                direct_role,
            };
            ops::take(u, req, ctx).await
        }
        UplinkCommand::GiveBack {
            holder_device_id,
            phone_key,
        } => ops::give_back(u, &holder_device_id, &phone_key).await,
        UplinkCommand::TakeBack { phone_confirmed } => ops::take_back(u, phone_confirmed).await,
        UplinkCommand::Status => ops::status(u),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn other_ops_fall_through_to_the_existing_parser() {
        assert!(parse(br#"{"op":"unpair"}"#).is_none());
        assert!(parse(b"not json").is_none());
    }

    #[test]
    fn a_take_carries_the_holder_the_key_and_the_yield_address() {
        let got = parse(
            br#"{"op":"uplink_take","holder_device_id":"a1b2c3d4e5f6","holder_name":"Pixel","phone_key":"AB12-CD34-EF56-7890","yield_url":"http://192.168.1.40:47811/uplink/yield"}"#,
        );
        assert_eq!(
            got,
            Some(Ok(UplinkCommand::Take(TakeRequest {
                holder_device_id: "a1b2c3d4e5f6".into(),
                holder_name: "Pixel".into(),
                phone_key: "AB12-CD34-EF56-7890".into(),
                yield_url: Some("http://192.168.1.40:47811/uplink/yield".into()),
            })))
        );
    }

    #[test]
    fn a_take_without_a_name_is_named_by_its_device_id() {
        let got = parse(br#"{"op":"uplink_take","holder_device_id":"a1b2","phone_key":"k"}"#);
        match got {
            Some(Ok(UplinkCommand::Take(t))) => assert_eq!(t.holder_name, "a1b2"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_take_or_give_back_without_the_phone_key_is_refused() {
        for line in [
            br#"{"op":"uplink_take","holder_device_id":"a1b2"}"#.as_slice(),
            br#"{"op":"uplink_give_back","holder_device_id":"a1b2"}"#.as_slice(),
        ] {
            match parse(line) {
                Some(Err(v)) => assert_eq!(v["error"], json!("E_BAD_REQUEST")),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn take_back_and_status_parse() {
        assert_eq!(
            parse(br#"{"op":"uplink_take_back","phone_confirmed":true}"#),
            Some(Ok(UplinkCommand::TakeBack {
                phone_confirmed: true
            }))
        );
        assert_eq!(
            parse(br#"{"op":"uplink_status"}"#),
            Some(Ok(UplinkCommand::Status))
        );
    }
}
