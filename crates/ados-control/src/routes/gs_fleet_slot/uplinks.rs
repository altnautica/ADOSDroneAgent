//! The uplinks a ground station offers a drone its address and relay secret on.
//!
//! A drone hears its ground on one ground `link_id` only: its own fleet's
//! (`link_id(fleet_id, SLOT_GROUND)`). An auto-pair bind carries no fleet id,
//! so the drone it pairs comes up on the default fleet, and a ground on any
//! other fleet reaches it only over its provisional aux uplink, keyed to the
//! default fleet's ground `link_id` (see `ados_groundlink::wfb_rx::
//! provisional_links`). Until the drone has taken the address, the reconciler
//! cannot know which of the two it hears, so each offer goes out on both at
//! once and the lane the drone answers on settles it; the other is never
//! answered and is dropped. The drone's answer comes back through the one
//! response path either way: it decodes onto a port the ground always binds,
//! and the provisional proxy shares the fleet proxy's pending calls
//! ([`AuxRpcProxy::over_egress`]).

use std::sync::Arc;

use ados_protocol::aux_rpc::{RequestParts, RpcMethod};
use ados_protocol::aux_rpc_proxy::AuxRpcProxy;

/// The ground station's relay uplinks.
#[derive(Clone)]
pub struct Uplinks {
    /// The ground's own fleet uplink, which every drone of its fleet hears.
    pub fleet: Arc<AuxRpcProxy>,
    /// The provisional uplink on the default fleet's ground `link_id`, `None`
    /// when it could not be opened.
    pub provisional: Option<Arc<AuxRpcProxy>>,
}

impl Uplinks {
    /// The lanes an offer from a ground on `fleet_id` rides: its own fleet's,
    /// and the provisional one while that is another fleet's (a ground on the
    /// default fleet hears and reaches an unaddressed drone on its own).
    pub fn lanes(&self, fleet_id: u16) -> Vec<Arc<AuxRpcProxy>> {
        let mut lanes = vec![Arc::clone(&self.fleet)];
        if ados_radio::config::provisional_fleet(fleet_id).is_some() {
            lanes.extend(self.provisional.iter().cloned());
        }
        lanes
    }
}

/// One relayed request, owned so each lane's call runs on its own task.
pub struct LaneRequest {
    pub device_id: String,
    pub method: RpcMethod,
    pub path: &'static [u8],
    pub content_type: &'static [u8],
    pub ticket: Vec<u8>,
    pub body: Vec<u8>,
}

/// Send `request` on every lane at once and return the HTTP status of the
/// first lane the drone answers on; the calls still waiting are dropped. `Err`
/// carries the last failure when no lane was answered (the drone is off, out
/// of range, or on neither address).
pub async fn call_on_lanes(
    lanes: Vec<Arc<AuxRpcProxy>>,
    request: LaneRequest,
) -> Result<u16, String> {
    let request = Arc::new(request);
    let mut calls = tokio::task::JoinSet::new();
    for lane in lanes {
        let request = Arc::clone(&request);
        calls.spawn(async move {
            lane.call_request(
                request.device_id.as_bytes(),
                request.method,
                &RequestParts {
                    path: request.path,
                    content_type: request.content_type,
                    ticket: &request.ticket,
                    body: &request.body,
                },
            )
            .await
            .map(|resp| resp.status)
            .map_err(|e| e.to_string())
        });
    }
    let mut last = "no uplink to call on".to_string();
    while let Some(joined) = calls.join_next().await {
        match joined {
            // Dropping the set aborts the lanes still waiting.
            Ok(Ok(status)) => return Ok(status),
            Ok(Err(e)) => last = e,
            Err(e) => last = format!("uplink call task failed: {e}"),
        }
    }
    Err(last)
}
