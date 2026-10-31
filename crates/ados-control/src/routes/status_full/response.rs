//! The typed `GET /api/status/full` body.
//!
//! Every key the handler always emits is a field here, under its exact wire
//! name (the body mixes snake_case and camelCase keys, so each camelCase field
//! carries its own rename). The nested blocks the route assembles from
//! sidecars, the store and systemd stay `Value`: their shapes belong to the
//! block builders. The sidecar folds and the router extras, whose key sets
//! depend on what is fresh on the node, ride in the flattened `extras` map.
//!
//! The schema derived from this type is committed at
//! `schemas/api/status-full.schema.json` and is the source of the generated
//! TypeScript `StatusFull` type.

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Map, Value};

use super::presence::PresenceFlags;

/// The consolidated status body. Field order is the wire order.
#[derive(Debug, Serialize, JsonSchema)]
pub struct StatusFullResponse {
    pub(crate) version: String,
    /// The service uptime from the router snapshot, else the process uptime.
    #[schemars(with = "f64")]
    pub(crate) uptime_seconds: Value,
    pub(crate) board: Value,
    #[serde(rename = "npuTops")]
    pub(crate) npu_tops: f64,
    #[serde(rename = "hasAccelerator")]
    pub(crate) has_accelerator: bool,
    #[serde(rename = "perceptionTier")]
    pub(crate) perception_tier: String,
    #[serde(rename = "perceptionOffloadTarget")]
    pub(crate) perception_offload_target: Option<String>,
    pub(crate) health: Value,
    pub(crate) fc_connected: bool,
    #[schemars(with = "String")]
    pub(crate) fc_port: Value,
    pub(crate) sim: bool,
    #[serde(flatten)]
    pub(crate) presence: PresenceFlags,
    pub(crate) lora: bool,
    #[schemars(with = "i64")]
    pub(crate) fc_baud: Value,
    #[serde(rename = "transportOpen")]
    pub(crate) transport_open: bool,
    #[serde(rename = "mavlinkAlive")]
    pub(crate) mavlink_alive: bool,
    #[serde(rename = "heartbeatAgeS")]
    #[schemars(with = "Option<f64>")]
    pub(crate) heartbeat_age_s: Value,
    #[serde(rename = "fcSource")]
    #[schemars(with = "String")]
    pub(crate) fc_source: Value,
    #[serde(rename = "fcLinkHint")]
    #[schemars(with = "String")]
    pub(crate) fc_link_hint: Value,
    #[serde(rename = "fcReachable")]
    pub(crate) fc_reachable: bool,
    #[serde(rename = "fcVariant")]
    #[schemars(with = "Option<String>")]
    pub(crate) fc_variant: Value,
    #[serde(rename = "fcFirmware")]
    #[schemars(with = "String")]
    pub(crate) fc_firmware: Value,
    #[serde(rename = "fcCommandDownGated")]
    pub(crate) fc_command_down_gated: bool,
    #[schemars(with = "Vec<Value>")]
    pub(crate) services: Value,
    pub(crate) resources: Value,
    pub(crate) video: Value,
    pub(crate) telemetry: Value,
    pub(crate) mesh: Value,
    pub(crate) radio: Value,
    pub(crate) profile: String,
    pub(crate) role: Option<String>,
    #[serde(rename = "runtimeMode")]
    pub(crate) runtime_mode: String,
    #[serde(rename = "relaySecretPresent")]
    pub(crate) relay_secret_present: bool,
    pub(crate) plugin_loopback_guard: String,
    /// The FC identity reconciler's last verdict, null before its first tick.
    pub(crate) fc_identity: Value,
    /// The swarm neighbour table, omitted when no swarm bus has published one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) neighbors: Option<Value>,
    /// The sidecar folds and the router extras.
    #[serde(flatten)]
    pub(crate) extras: Map<String, Value>,
}
