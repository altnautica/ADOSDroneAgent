//! The uplinks a ground station offers a drone its address and relay secret on.
//!
//! A drone hears its ground on one ground `link_id` only: its own fleet's
//! (`link_id(fleet_id, SLOT_GROUND)`). An auto-pair bind carries no fleet id,
//! so the drone it pairs comes up on the default fleet, and a ground on any
//! other fleet reaches it only over its provisional aux uplink, keyed to the
//! default fleet's ground `link_id` (see `ados_groundlink::wfb_rx::
//! provisional_links`).
//!
//! The provisional uplink carries an offer only for a drone the ground hears
//! waiting on the provisional lane right now (its fresh beacon, marked
//! `provisional` in the linked-peers sidecar). Every other drone, an offline
//! one included, is offered on the fleet uplink alone: the default fleet's
//! ground `link_id` is every default-fleet drone's uplink, and frames on it
//! keyed with this ground's key are undecodable noise to a nearby drone paired
//! to another ground, which reads them as a mis-keyed link. For a waiting drone
//! the offer goes out on both at once (it may move between ticks) and the lane
//! it answers on settles it. Its answer comes back through the one response
//! path: the provisional aux consumer forwards relay answers, and the
//! provisional proxy shares the fleet proxy's pending calls
//! ([`AuxRpcProxy::over_egress`]).

use std::collections::BTreeSet;
use std::path::Path;
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
    /// The lanes an offer from a ground on `fleet_id` rides to a drone: its
    /// own fleet's, and the provisional one only when `waiting` (the drone is
    /// heard on the provisional lane now) and that lane is another fleet's (a
    /// ground on the default fleet reaches such a drone on its own uplink).
    pub fn lanes(&self, fleet_id: u16, waiting: bool) -> Vec<Arc<AuxRpcProxy>> {
        let mut lanes = vec![Arc::clone(&self.fleet)];
        if waiting && ados_radio::config::provisional_fleet(fleet_id).is_some() {
            lanes.extend(self.provisional.iter().cloned());
        }
        lanes
    }
}

/// Device ids heard waiting on the provisional lane: fresh rows of the
/// linked-peers sidecar at `sidecar` marked `provisional`.
pub(crate) fn waiting_drones(sidecar: &Path, now: f64) -> BTreeSet<String> {
    crate::routes::status_full::fresh_linked_peer_rows_in(sidecar, now)
        .into_iter()
        .filter(|p| p.provisional)
        .map(|p| p.device_id)
        .collect()
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
