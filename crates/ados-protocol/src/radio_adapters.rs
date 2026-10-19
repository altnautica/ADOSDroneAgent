//! Per-adapter WFB radio report: the `adapters[]` + `diversity` blocks of the
//! `wfb-stats` sidecar (version 2) and the body of `GET /api/v1/radio/adapters`.
//!
//! A ground station can run several WFB adapters at once. Every adapter with a
//! receive role is handed to one `wfb_rx`, which combines their frames before
//! FEC decode (receive diversity); one adapter carries the transmit lanes. The
//! receive plane publishes one [`RadioAdapterReport`] per adapter plus a
//! [`DiversityReport`] comparing the combined loss against the best single
//! adapter over the same window. The route serves the same objects verbatim.
//!
//! Producer: `ados-groundlink` (the receive plane's stats reader). Consumers:
//! `ados-control` (`/api/v1/radio/adapters`).

use serde::{Deserialize, Serialize};

/// What an adapter is used for.
///
/// `rxtx` receives and carries the transmit lanes, `rx` only receives, `tx`
/// only transmits, and `mesh` is left out of WFB entirely for the batman-adv
/// mesh carrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AdapterRole {
    Rxtx,
    Rx,
    Tx,
    Mesh,
}

impl AdapterRole {
    /// The wire string (`rxtx`, `rx`, `tx`, `mesh`).
    pub fn as_str(self) -> &'static str {
        match self {
            AdapterRole::Rxtx => "rxtx",
            AdapterRole::Rx => "rx",
            AdapterRole::Tx => "tx",
            AdapterRole::Mesh => "mesh",
        }
    }

    /// True for the roles whose interface is handed to `wfb_rx`.
    pub fn receives(self) -> bool {
        matches!(self, AdapterRole::Rxtx | AdapterRole::Rx)
    }
}

/// One adapter's identity, role and receive counters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RadioAdapterReport {
    pub iface: String,
    /// The adapter's MAC address, `None` when it could not be read.
    pub mac: Option<String>,
    /// The USB device path (`1-1.2`), `None` when the adapter is not USB-backed.
    pub usb_path: Option<String>,
    pub driver: String,
    pub role: AdapterRole,
    /// The operator's antenna label from config, `None` when unset.
    pub antenna: Option<String>,
    /// True for the adapter carrying the transmit lanes.
    pub tx_active: bool,
    /// Average RSSI per receive chain over the last stats interval, in chain
    /// order. Empty when the adapter received nothing in that interval.
    pub rssi_dbm: Vec<i32>,
    /// Frames this adapter received since the receive chain started.
    pub packets: u64,
    /// Frames this adapter missed since the receive chain started.
    pub lost: u64,
    /// FEC recoveries attributable to this adapter. The decoder reports one
    /// combined count, so this is only set when a single adapter receives.
    pub fec_rec: Option<u64>,
    /// Monitor mode was set and read back on this adapter.
    pub verified: bool,
}

/// Combined-versus-single receive loss over one window.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DiversityReport {
    /// Adapters handed to `wfb_rx` (`rx` and `rxtx` roles).
    pub rx_adapters: u32,
    /// Loss after combining every receive adapter, in percent. `None` until a
    /// window carried traffic.
    pub combined_loss_pct: Option<f64>,
    /// Loss of the best single receive adapter over the same window, in
    /// percent. `None` until a window carried traffic.
    pub best_single_loss_pct: Option<f64>,
}

/// The `{adapters, diversity}` body. Default is the honest empty shape served
/// when no receive plane is reporting.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RadioAdaptersBody {
    #[serde(default)]
    pub adapters: Vec<RadioAdapterReport>,
    #[serde(default)]
    pub diversity: DiversityReport,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_empty_body_matches_the_absent_shape() {
        let v = serde_json::to_value(RadioAdaptersBody::default()).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "adapters": [],
                "diversity": {
                    "rx_adapters": 0,
                    "combined_loss_pct": null,
                    "best_single_loss_pct": null,
                },
            })
        );
    }

    #[test]
    fn roles_use_the_lowercase_wire_names() {
        for role in [
            AdapterRole::Rxtx,
            AdapterRole::Rx,
            AdapterRole::Tx,
            AdapterRole::Mesh,
        ] {
            assert_eq!(
                serde_json::to_value(role).unwrap(),
                serde_json::json!(role.as_str())
            );
        }
        assert!(AdapterRole::Rxtx.receives());
        assert!(AdapterRole::Rx.receives());
        assert!(!AdapterRole::Tx.receives());
        assert!(!AdapterRole::Mesh.receives());
    }
}
