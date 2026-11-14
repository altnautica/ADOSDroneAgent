//! Framing and op-keyed (de)serialisation for `pilot.v1`.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{ActionReq, Goal, Holder, Input, Request, ViewDemand, PILOT_MAX_FRAME};
use crate::frame::encode_frame;

/// Why a `pilot.v1` message could not be read or decoded.
#[derive(Debug, thiserror::Error)]
pub enum PilotWireError {
    #[error("pilot frame: {0}")]
    Frame(#[from] crate::frame::FrameError),
    #[error("pilot io: {0}")]
    Io(#[from] std::io::Error),
    #[error("pilot message is not a msgpack map with a string `op`")]
    NoOp,
    #[error("pilot message `{op}` is malformed: {err}")]
    Malformed { op: String, err: String },
    #[error("pilot message encode: {0}")]
    Encode(String),
}

/// Read one length-prefixed body. `Ok(None)` on a clean EOF before a header.
pub async fn read_msg<R: AsyncReadExt + Unpin>(
    r: &mut R,
) -> Result<Option<Vec<u8>>, PilotWireError> {
    Ok(crate::ipc::read_length_prefixed(r, PILOT_MAX_FRAME, true).await?)
}

/// Write one body with its length prefix.
pub async fn write_msg<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    body: &[u8],
) -> Result<(), PilotWireError> {
    let framed = encode_frame(body, PILOT_MAX_FRAME)?;
    w.write_all(&framed).await?;
    Ok(())
}

/// Encode `body` (a struct or map) as a msgpack map with `op` added first.
pub fn encode_op<T: Serialize + ?Sized>(op: &str, body: &T) -> Result<Vec<u8>, PilotWireError> {
    let named = rmp_serde::to_vec_named(body).map_err(|e| PilotWireError::Encode(e.to_string()))?;
    let value = rmpv::decode::read_value(&mut named.as_slice())
        .map_err(|e| PilotWireError::Encode(e.to_string()))?;
    let mut entries = vec![(rmpv::Value::from("op"), rmpv::Value::from(op))];
    match value {
        rmpv::Value::Map(m) => {
            entries.extend(m.into_iter().filter(|(k, _)| k.as_str() != Some("op")))
        }
        rmpv::Value::Nil => {}
        other => {
            return Err(PilotWireError::Encode(format!(
                "body is not a map: {other}"
            )))
        }
    }
    let mut out = Vec::new();
    rmpv::encode::write_value(&mut out, &rmpv::Value::Map(entries))
        .map_err(|e| PilotWireError::Encode(e.to_string()))?;
    Ok(out)
}

/// The `op` of a msgpack body, and the decoded map.
pub fn peek_op(body: &[u8]) -> Result<(String, rmpv::Value), PilotWireError> {
    let value = rmpv::decode::read_value(&mut &body[..]).map_err(|_| PilotWireError::NoOp)?;
    let op = match &value {
        rmpv::Value::Map(m) => m
            .iter()
            .find(|(k, _)| k.as_str() == Some("op"))
            .and_then(|(_, v)| v.as_str())
            .map(str::to_owned),
        _ => None,
    };
    op.map(|op| (op, value)).ok_or(PilotWireError::NoOp)
}

fn field<T: DeserializeOwned>(op: &str, value: rmpv::Value) -> Result<T, PilotWireError> {
    let malformed = |err: String| PilotWireError::Malformed {
        op: op.to_owned(),
        err,
    };
    let mut bytes = Vec::new();
    rmpv::encode::write_value(&mut bytes, &value).map_err(|e| malformed(e.to_string()))?;
    rmp_serde::from_slice(&bytes).map_err(|e| malformed(e.to_string()))
}

#[derive(Deserialize)]
struct SrcSession {
    src: String,
    session: u32,
}
#[derive(Deserialize)]
struct Src {
    #[serde(default)]
    src: String,
}
#[derive(Deserialize)]
struct LeaseAcq {
    holder: Holder,
    #[serde(default = "motion")]
    kind: String,
}
fn motion() -> String {
    "motion".to_owned()
}
#[derive(Deserialize)]
struct LeaseRel {
    lease_id: u64,
}
#[derive(Deserialize)]
struct GoalReq {
    lease_id: u64,
    goal: Goal,
}
#[derive(Deserialize)]
struct GotoReq {
    n: f32,
    e: f32,
    d: f32,
    #[serde(default)]
    yaw: Option<f32>,
}
#[derive(Deserialize)]
struct SwarmReq {
    vn: f32,
    ve: f32,
    vd: f32,
    #[serde(default)]
    yaw: Option<f32>,
}
#[derive(Deserialize)]
struct FeedReq {
    topic: String,
    payload: rmpv::Value,
}

/// Decode a client request body.
pub fn decode_request(body: &[u8]) -> Result<Request, PilotWireError> {
    let (op, value) = peek_op(body)?;
    let req = match op.as_str() {
        "input" => Request::Input(field::<Input>(&op, value)?),
        "release" => {
            let s: SrcSession = field(&op, value)?;
            Request::Release {
                src: s.src,
                session: s.session,
            }
        }
        "session.take" => {
            let s: SrcSession = field(&op, value)?;
            Request::SessionTake {
                src: s.src,
                session: s.session,
            }
        }
        "hb" => Request::Hb {
            src: field::<Src>(&op, value)?.src,
        },
        "action" => Request::Action(field::<ActionReq>(&op, value)?),
        "lease.acquire" => {
            let l: LeaseAcq = field(&op, value)?;
            Request::LeaseAcquire {
                holder: l.holder,
                kind: l.kind,
            }
        }
        "lease.release" => Request::LeaseRelease {
            lease_id: field::<LeaseRel>(&op, value)?.lease_id,
        },
        "goal" => {
            let g: GoalReq = field(&op, value)?;
            Request::Goal {
                lease_id: g.lease_id,
                goal: g.goal,
            }
        }
        "goto" => {
            let g: GotoReq = field(&op, value)?;
            Request::Goto {
                n: g.n,
                e: g.e,
                d: g.d,
                yaw: g.yaw,
            }
        }
        "subscribe" => Request::Subscribe,
        "swarm.goal" => {
            let g: SwarmReq = field(&op, value)?;
            Request::SwarmGoal {
                vn: g.vn,
                ve: g.ve,
                vd: g.vd,
                yaw: g.yaw,
            }
        }
        "view_demand" => Request::ViewDemand(field::<ViewDemand>(&op, value)?),
        "feed" => {
            let f: FeedReq = field(&op, value)?;
            Request::Feed {
                topic: f.topic,
                payload: f.payload,
            }
        }
        _ => Request::Other { op, body: value },
    };
    Ok(req)
}

/// Re-encode a JSON client frame (as a surface sends it over the pilot
/// WebSocket) as a `pilot.v1` msgpack body, after checking it decodes.
pub fn json_to_request_body(
    frame: &serde_json::Value,
) -> Result<(Request, Vec<u8>), PilotWireError> {
    let body = rmp_serde::to_vec_named(frame).map_err(|e| PilotWireError::Encode(e.to_string()))?;
    let req = decode_request(&body)?;
    Ok((req, body))
}

/// Reply to `lease.acquire`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseReply {
    pub ok: bool,
    #[serde(default)]
    pub lease_id: Option<u64>,
    #[serde(default)]
    pub err: Option<String>,
    #[serde(default)]
    pub holder: Option<Holder>,
}

/// Reply to `action`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionResult {
    pub name: String,
    #[serde(default)]
    pub id: Option<String>,
    pub ok: bool,
    #[serde(default)]
    pub err: Option<String>,
}

/// The `sticks` message sent to subscribers while a skill owns motion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sticks {
    pub lease_id: u64,
    pub axes: [f32; 4],
}

/// The `lease.revoked` message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseRevoked {
    pub lease_id: u64,
    pub reason: String,
}

/// Progress toward a `vantage` goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum VantageState {
    /// Flying to the vantage point.
    Enroute,
    /// Within 1 m of the vantage point and nearly still.
    Arrived,
    /// No reachable point inside the search bound sees the area.
    NoVantage,
}

/// The `goal.status` message, published at every replan while the lease
/// goal is a `vantage` goal. The plugin host forwards it to the holder only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GoalStatus {
    pub lease_id: u64,
    pub state: VantageState,
    #[serde(default)]
    pub vantage_ne: Option<[f32; 2]>,
    /// Fraction of the sample points in line of sight from the vantage point (0..1).
    pub coverage: f32,
    #[serde(default)]
    pub occluder_ne: Option<[f32; 2]>,
    #[serde(default)]
    pub occluder_width_m: Option<f32>,
}

impl GoalStatus {
    /// A status with no vantage point and no occluder; set them on the
    /// returned value.
    pub fn new(lease_id: u64, state: VantageState, coverage: f32) -> Self {
        Self {
            lease_id,
            state,
            vantage_ne: None,
            coverage,
            occluder_ne: None,
            occluder_width_m: None,
        }
    }
}

#[cfg(test)]
mod tests;
