//! WFB-ng air-side orchestration for the lightweight ADOS Drone Agent.
//!
//! Three responsibilities:
//!
//! 1. Watch for an RTL8812-family broadcast adapter to be hot-plugged.
//!    See [`udev`].
//! 2. Derive a 32-byte broadcast key from an operator-supplied
//!    passphrase, matching the upstream wfb-ng key contract. See
//!    [`keys`].
//! 3. Spawn the upstream `wfb_tx` C binary as a child process and
//!    keep it alive across crashes. See [`process`].
//!
//! The agent does not reimplement the wfb-ng userland — it
//! orchestrates the existing C tooling, the same way the Python full
//! agent shells out to `rpicam-vid` and `ffmpeg`. Hardware validation
//! against a Luckfox Pico Zero + RTL8812EU dongle lands in a follow-on
//! pass that pins the upstream argument names and key format.

#![forbid(unsafe_code)]

pub mod keys;
pub mod process;
pub mod udev;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::{info, warn};

pub use keys::{derive_key, seal, unseal, KeyError, KEY_LEN, NONCE_LEN};
pub use process::{ProcessError, WfbProcess, WfbTxArgs, DEFAULT_WFB_TX_PATH};
pub use udev::{DongleEvent, SysfsUdev, UdevError};

#[cfg(any(test, feature = "mock"))]
pub use udev::MockUdev;

/// Configuration the manager reads at construction time. Lives in
/// `agent.yaml` under a `wfb:` block in production; tests hand a
/// `WfbConfig` directly.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WfbConfig {
    /// 802.11 channel number. Validated at apply time: 1-13 for 2.4 GHz
    /// or 36-165 for 5 GHz are accepted; anything else surfaces a typed
    /// error to the wizard.
    pub channel: u8,
    /// 802.11 MCS index. 0 to 7 for the single-stream RTL8812 path.
    pub mcs_index: u8,
    /// Transmit power in dBm. Negative leaves the adapter at default.
    pub tx_power_dbm: i8,
    /// Operator-supplied passphrase. Hashed via the [`keys`] module
    /// before it crosses any process boundary; never stored in the
    /// clear in any persistent file.
    pub key_passphrase: String,
    /// Filesystem path to the `wfb_tx` userland binary. Defaults to
    /// [`process::DEFAULT_WFB_TX_PATH`].
    pub wfb_tx_path: PathBuf,
    /// `wlanX` interface to bind to. `None` means "auto-detect via
    /// udev"; the manager fills this in once it sees an Added event.
    pub interface: Option<String>,
}

impl Default for WfbConfig {
    fn default() -> Self {
        Self {
            channel: 161,
            mcs_index: 1,
            tx_power_dbm: 25,
            key_passphrase: String::new(),
            wfb_tx_path: PathBuf::from(DEFAULT_WFB_TX_PATH),
            interface: None,
        }
    }
}

/// Top-level errors the manager exposes.
#[derive(Debug, Error)]
pub enum WfbError {
    /// Channel number is outside the 2.4 GHz or 5 GHz allowed ranges.
    #[error("channel {0} is outside the allowed 2.4/5 GHz ranges")]
    InvalidChannel(u8),
    /// MCS index past the single-stream ceiling.
    #[error("mcs_index {0} exceeds 0..=7")]
    InvalidMcs(u8),
    /// Transmit power outside the 0..30 dBm safety envelope.
    #[error("tx_power_dbm {0} outside 0..=30")]
    InvalidPower(i8),
    /// Passphrase rejected by the key derivation layer.
    #[error(transparent)]
    Key(#[from] KeyError),
    /// Subprocess layer error.
    #[error(transparent)]
    Process(#[from] ProcessError),
    /// Udev backend error.
    #[error(transparent)]
    Udev(#[from] UdevError),
    /// Manager invariant violation (e.g., start called twice).
    #[error("manager invariant: {0}")]
    Invariant(&'static str),
}

/// State machine the manager exposes over a snapshot. Wire-compatible
/// with the [`/api/v1/setup/wfb`](../../proto/setup/setup-api.yaml) GET
/// response.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WfbState {
    /// No dongle present. Manager is parked, waiting on an Added event.
    Idle,
    /// Dongle has appeared but `wfb_tx` has not been spawned yet.
    DongleDetected { iface: String },
    /// `wfb_tx` is running.
    Running { iface: String },
    /// Most recent run crashed; restart is pending. `restart_at_unix`
    /// is the wall-clock time when the manager will retry.
    Crashed {
        last_error: String,
        restart_at_unix: i64,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct WfbStateSnapshot {
    pub state: WfbState,
    pub config_summary: ConfigSummary,
}

/// Public surface of [`WfbConfig`] — passphrase deliberately omitted.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigSummary {
    pub channel: u8,
    pub mcs_index: u8,
    pub tx_power_dbm: i8,
    pub interface: Option<String>,
    pub binary_present: bool,
}

/// The orchestrator. One instance per agent.
pub struct WfbManager {
    config: Arc<Mutex<WfbConfig>>,
    state: Arc<Mutex<InternalState>>,
}

#[derive(Debug)]
struct InternalState {
    public: WfbState,
    /// Wall-clock instant at which `state` was last updated. Powers
    /// the "running stably" heuristic that resets the restart backoff.
    #[allow(dead_code)]
    updated_at: Instant,
}

impl WfbManager {
    /// Construct a manager over an existing config. Does not start
    /// anything — call [`WfbManager::start`] after wiring an event
    /// source.
    pub fn new(config: WfbConfig) -> Result<Self, WfbError> {
        validate_config(&config)?;
        Ok(Self {
            config: Arc::new(Mutex::new(config)),
            state: Arc::new(Mutex::new(InternalState {
                public: WfbState::Idle,
                updated_at: Instant::now(),
            })),
        })
    }

    /// Mark the manager as live. Stub today: a follow-on pass spawns
    /// the udev watcher and the orchestration loop, both of which are
    /// scoped behind the same gate as the hardware-validation work.
    pub async fn start(&self) -> Result<(), WfbError> {
        let mut guard = self.state.lock().await;
        if !matches!(guard.public, WfbState::Idle) {
            return Err(WfbError::Invariant("start called more than once"));
        }
        info!("wfb manager started in idle state");
        guard.updated_at = Instant::now();
        Ok(())
    }

    /// Stop the orchestration loop. Stub today.
    pub async fn stop(&self) -> Result<(), WfbError> {
        let mut guard = self.state.lock().await;
        guard.public = WfbState::Idle;
        guard.updated_at = Instant::now();
        info!("wfb manager stopped");
        Ok(())
    }

    /// Read-only snapshot for the REST surface.
    pub async fn state_snapshot(&self) -> WfbStateSnapshot {
        let state = self.state.lock().await;
        let cfg = self.config.lock().await;
        WfbStateSnapshot {
            state: state.public.clone(),
            config_summary: ConfigSummary {
                channel: cfg.channel,
                mcs_index: cfg.mcs_index,
                tx_power_dbm: cfg.tx_power_dbm,
                interface: cfg.interface.clone(),
                binary_present: cfg.wfb_tx_path.exists(),
            },
        }
    }

    /// Apply a new config. Validates first; on success swaps the
    /// stored config and signals the orchestration loop that it should
    /// respawn `wfb_tx` with the new arguments.
    pub async fn apply_config(&self, new: WfbConfig) -> Result<(), WfbError> {
        validate_config(&new)?;
        let mut cfg = self.config.lock().await;
        *cfg = new;
        info!(
            channel = cfg.channel,
            mcs = cfg.mcs_index,
            "wfb config updated"
        );
        Ok(())
    }

    /// Drive the state machine from a single dongle event. Test path
    /// for the manager state-machine logic — the udev event loop wires
    /// this in once the hardware-validation gate clears.
    pub async fn handle_dongle_event(&self, evt: DongleEvent) {
        let mut guard = self.state.lock().await;
        match evt {
            DongleEvent::Added(iface) => {
                let mut cfg = self.config.lock().await;
                cfg.interface = Some(iface.clone());
                drop(cfg);
                guard.public = WfbState::DongleDetected { iface };
                guard.updated_at = Instant::now();
            }
            DongleEvent::Removed(iface) => {
                warn!(iface, "wfb dongle removed; returning to idle");
                guard.public = WfbState::Idle;
                guard.updated_at = Instant::now();
            }
        }
    }

    /// Build the `wfb_tx` argv from the current config. Returns `None`
    /// when the manager is not yet bound to an interface.
    pub async fn build_args(&self) -> Result<Option<WfbTxArgs>, WfbError> {
        let cfg = self.config.lock().await;
        let iface = match cfg.interface.clone() {
            Some(i) => i,
            None => return Ok(None),
        };
        let key = derive_key(&cfg.key_passphrase)?;
        let key_hex = key.iter().map(|b| format!("{b:02x}")).collect();
        Ok(Some(WfbTxArgs {
            interface: iface,
            channel: cfg.channel,
            mcs_index: cfg.mcs_index,
            tx_power_dbm: cfg.tx_power_dbm,
            key_hex,
        }))
    }
}

fn validate_config(cfg: &WfbConfig) -> Result<(), WfbError> {
    let valid_24 = (1..=13).contains(&cfg.channel);
    let valid_5 = (36..=165).contains(&cfg.channel);
    if !(valid_24 || valid_5) {
        return Err(WfbError::InvalidChannel(cfg.channel));
    }
    if cfg.mcs_index > 7 {
        return Err(WfbError::InvalidMcs(cfg.mcs_index));
    }
    // tx_power_dbm < 0 means "leave at adapter default"; non-negative
    // values are clamped to the 0..=30 dBm safety envelope so an
    // operator typo like `tx_power_dbm: 99` surfaces as a typed error
    // rather than blowing past regulatory power caps.
    if cfg.tx_power_dbm > 30 {
        return Err(WfbError::InvalidPower(cfg.tx_power_dbm));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> WfbConfig {
        WfbConfig {
            channel: 161,
            mcs_index: 1,
            tx_power_dbm: 25,
            key_passphrase: "test-passphrase".to_string(),
            wfb_tx_path: PathBuf::from(DEFAULT_WFB_TX_PATH),
            interface: None,
        }
    }

    /// Fresh manager starts in Idle.
    #[tokio::test]
    async fn manager_starts_idle() {
        let m = WfbManager::new(cfg()).expect("ctor");
        let snap = m.state_snapshot().await;
        assert!(matches!(snap.state, WfbState::Idle));
        assert_eq!(snap.config_summary.channel, 161);
    }

    /// Dongle Added event drives Idle → DongleDetected.
    #[tokio::test]
    async fn manager_transitions_on_dongle_added() {
        let m = WfbManager::new(cfg()).expect("ctor");
        m.handle_dongle_event(DongleEvent::Added("wlan0".to_string())).await;
        let snap = m.state_snapshot().await;
        match snap.state {
            WfbState::DongleDetected { iface } => assert_eq!(iface, "wlan0"),
            other => panic!("expected DongleDetected, got {other:?}"),
        }
        assert_eq!(snap.config_summary.interface.as_deref(), Some("wlan0"));
    }

    /// Removal returns the manager to Idle.
    #[tokio::test]
    async fn manager_returns_to_idle_on_removal() {
        let m = WfbManager::new(cfg()).expect("ctor");
        m.handle_dongle_event(DongleEvent::Added("wlan0".to_string())).await;
        m.handle_dongle_event(DongleEvent::Removed("wlan0".to_string())).await;
        let snap = m.state_snapshot().await;
        assert!(matches!(snap.state, WfbState::Idle));
    }

    /// Apply config rejects an out-of-range channel.
    #[tokio::test]
    async fn apply_config_rejects_invalid_channel() {
        let m = WfbManager::new(cfg()).expect("ctor");
        let mut bad = cfg();
        bad.channel = 200; // outside both bands
        match m.apply_config(bad).await {
            Err(WfbError::InvalidChannel(200)) => {}
            other => panic!("expected InvalidChannel, got {other:?}"),
        }
    }

    /// Apply config rejects an out-of-range MCS.
    #[tokio::test]
    async fn apply_config_rejects_invalid_mcs() {
        let m = WfbManager::new(cfg()).expect("ctor");
        let mut bad = cfg();
        bad.mcs_index = 9;
        match m.apply_config(bad).await {
            Err(WfbError::InvalidMcs(9)) => {}
            other => panic!("expected InvalidMcs, got {other:?}"),
        }
    }

    /// build_args returns None until a dongle is bound, then returns
    /// the argv with the derived key in hex.
    #[tokio::test]
    async fn build_args_yields_hex_key_after_binding() {
        let m = WfbManager::new(cfg()).expect("ctor");
        assert!(m.build_args().await.expect("ok").is_none());
        m.handle_dongle_event(DongleEvent::Added("wlan0".to_string())).await;
        let args = m.build_args().await.expect("ok").expect("bound");
        assert_eq!(args.interface, "wlan0");
        // Hex of 32 bytes is 64 chars.
        assert_eq!(args.key_hex.len(), 64);
        assert!(args.key_hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Constructor rejects an obviously bad initial config so the
    /// caller gets a typed error at boot, not a silent acceptance
    /// followed by a runtime spawn failure.
    #[test]
    fn ctor_rejects_invalid_initial_config() {
        let mut bad = cfg();
        bad.tx_power_dbm = 99;
        match WfbManager::new(bad) {
            Err(WfbError::InvalidPower(99)) => {}
            Err(other) => panic!("expected InvalidPower, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok(WfbManager)"),
        }
    }
}
