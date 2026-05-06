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

use ados_video::EncodedFrame;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, Mutex};
use tracing::{debug, info, warn};

/// Type-erased async writer the manager pipes encoded frames into. In
/// production the supervisor stuffs a `tokio::process::ChildStdin` here
/// once `wfb_tx` is running; tests use an in-memory writer (e.g. a
/// `tokio::io::DuplexStream` half) so the tee path can be exercised
/// without a real subprocess.
pub type WfbTxStdin = Box<dyn AsyncWrite + Send + Unpin>;

pub use keys::{
    derive_key, derive_keypair, generate_keypair, generate_passphrase, key_fingerprint,
    regenerate_public_key_hex, seal, unseal, KeyError, KEY_LEN, NONCE_LEN, PUBLIC_KEY_LEN,
};
pub use process::{
    GuardInterval, ProcessError, WfbAdvancedOpts, WfbProcess, WfbTxArgs, DEFAULT_WFB_TX_PATH,
};
pub use udev::{spawn_udev, DongleEvent, SysfsUdev, UdevBackend, UdevError};

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
    ///
    /// An empty string means "keep the existing keypair file on disk
    /// untouched": no derive runs, no key persistence happens, and
    /// `wfb_tx` keeps reading the bytes that are already at
    /// `keypair_path`. This is the path operators take when they want
    /// to retune channel/MCS/power without rotating the broadcast
    /// secret.
    pub key_passphrase: String,
    /// Filesystem path to the `wfb_tx` userland binary. Defaults to
    /// [`process::DEFAULT_WFB_TX_PATH`].
    pub wfb_tx_path: PathBuf,
    /// `wlanX` interface to bind to. `None` means "auto-detect via
    /// udev"; the manager fills this in once it sees an Added event.
    pub interface: Option<String>,
    /// Filesystem path to the keypair file (32-byte secret + 32-byte
    /// public concatenated). Read by `wfb_tx` via the `-K` flag. Mode
    /// 0600 owned by root; the manager refuses to spawn against a
    /// world-readable file.
    pub keypair_path: PathBuf,
    /// FEC + PHY tuning passed through to `wfb_tx`. Defaults match the
    /// upstream example config (`fec_k=8 fec_n=12 -B 20 -G long ...`).
    #[serde(default)]
    pub advanced: WfbAdvancedOpts,
}

/// Default keypair location. Lives under the same `/etc/ados/secrets/`
/// directory as the other agent secrets so the install script's 0700
/// permissions enforcement covers it transparently.
pub const DEFAULT_KEYPAIR_PATH: &str = "/etc/ados/secrets/wfb-keypair";

impl Default for WfbConfig {
    fn default() -> Self {
        Self {
            channel: 161,
            mcs_index: 1,
            tx_power_dbm: 25,
            key_passphrase: String::new(),
            wfb_tx_path: PathBuf::from(DEFAULT_WFB_TX_PATH),
            interface: None,
            keypair_path: PathBuf::from(DEFAULT_KEYPAIR_PATH),
            advanced: WfbAdvancedOpts::default(),
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
    /// Filesystem I/O failure on a runtime path (keypair persistence,
    /// etc.). Wraps the underlying `std::io::Error` so the wizard can
    /// surface a useful operator hint.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
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
    /// Async writer that points at the running `wfb_tx` child's stdin.
    /// `None` when no child is alive — frames sent through the tee path
    /// are silently dropped in that state. The supervisor that owns the
    /// subprocess installs and clears the handle through
    /// [`WfbManager::set_wfb_tx_stdin`] across the spawn / restart
    /// boundary so [`WfbManager::tee_to_wfb_tx`] never holds a stale
    /// child stdin past the subprocess's lifetime.
    wfb_tx_stdin: Arc<Mutex<Option<WfbTxStdin>>>,
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
            wfb_tx_stdin: Arc::new(Mutex::new(None)),
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
        // When a passphrase is set, validate it up-front so a malformed
        // key surfaces as a typed error rather than a SpawnFailed at
        // exec time. An empty passphrase signals "keep the existing
        // keypair file on disk", so the validate step is skipped — the
        // file itself becomes the source of truth and `wfb_tx` reads
        // the bytes already there via `-K`.
        if !cfg.key_passphrase.is_empty() {
            let _ = derive_key(&cfg.key_passphrase)?;
        }
        Ok(Some(WfbTxArgs {
            interface: iface,
            channel: cfg.channel,
            mcs_index: cfg.mcs_index,
            tx_power_dbm: cfg.tx_power_dbm,
            keypair_path: cfg.keypair_path.clone(),
            advanced: cfg.advanced.clone(),
        }))
    }

    /// Materialise the keypair file at `cfg.keypair_path`. Writes the
    /// 32-byte secret derived from the passphrase followed by its
    /// 32-byte public component, matching the keypair-file format
    /// `wfb_tx -K` consumes. The file is created with mode 0600; the
    /// caller is expected to have already locked the parent directory
    /// to 0700 via `ensure_secret_dir` from the setup crate.
    ///
    /// Requires a non-empty passphrase in the in-memory config. The
    /// "keep current keypair" path is served by
    /// [`WfbManager::persist_keypair_if_passphrase_set`] which short-
    /// circuits to a no-op when the passphrase is empty.
    pub async fn persist_keypair_file(&self) -> Result<[u8; PUBLIC_KEY_LEN], WfbError> {
        let cfg = self.config.lock().await;
        let (public, broadcast) = derive_keypair(&cfg.key_passphrase)?;
        let mut bytes = Vec::with_capacity(KEY_LEN + PUBLIC_KEY_LEN);
        bytes.extend_from_slice(&broadcast);
        bytes.extend_from_slice(&public);
        write_keypair_atomic(&cfg.keypair_path, &bytes)?;
        Ok(public)
    }

    /// Persist the keypair file only when the in-memory passphrase is
    /// set. With an empty passphrase the call is a no-op and the
    /// existing keypair file on disk is left untouched, which is the
    /// path operators take when they retune channel/MCS/power without
    /// rotating the broadcast secret. Returns `Ok(None)` in that case;
    /// returns the freshly-minted public bytes when a write actually
    /// happened.
    pub async fn persist_keypair_if_passphrase_set(
        &self,
    ) -> Result<Option<[u8; PUBLIC_KEY_LEN]>, WfbError> {
        let is_empty = self.config.lock().await.key_passphrase.is_empty();
        if is_empty {
            return Ok(None);
        }
        let public = self.persist_keypair_file().await?;
        Ok(Some(public))
    }

    /// Install (or clear) the writer that points at the live `wfb_tx`
    /// child's stdin. The supervisor calls this with `Some(stdin)` after
    /// a successful spawn and with `None` before a restart so the tee
    /// loop drops frames cleanly across the gap rather than panicking on
    /// a half-closed pipe.
    pub async fn set_wfb_tx_stdin(&self, stdin: Option<WfbTxStdin>) {
        let mut guard = self.wfb_tx_stdin.lock().await;
        *guard = stdin;
    }

    /// Drive the encoder broadcast channel into the running `wfb_tx`
    /// subprocess.
    ///
    /// Each access unit from the encoder is written as one Annex-B byte
    /// stream in a single `write_all` call. When no subprocess is
    /// running (`set_wfb_tx_stdin(None)`) frames are silently dropped:
    /// the tee never buffers and never blocks the encoder. When the
    /// stdin pipe errors out (broken pipe on `wfb_tx` exit) the writer
    /// is cleared and the loop continues, so the next supervisor restart
    /// can install a fresh handle without a dangling FD on the manager.
    ///
    /// Lagged frames produce a warn-level log line and are skipped per
    /// `tokio::sync::broadcast` semantics. The function returns when
    /// the broadcast `Sender` is dropped (encoder pipeline exit).
    pub async fn tee_to_wfb_tx(
        &self,
        mut rx: broadcast::Receiver<EncodedFrame>,
    ) -> Result<(), WfbError> {
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    // Lock briefly, take a snapshot of whether a writer
                    // is installed, write while holding the lock, then
                    // drop. The write itself is a single Annex-B blob;
                    // the upstream `wfb_tx` reads framed messages from
                    // its UDP socket in production, so writing through
                    // stdin is the supervised pre-encode tee path used
                    // by the air-side recorder + future debug shims.
                    let mut guard = self.wfb_tx_stdin.lock().await;
                    let writer = match guard.as_mut() {
                        Some(w) => w,
                        None => {
                            // No subprocess attached. Drop the frame
                            // silently — the encoder must NOT stall on
                            // an offline tee. The broadcast channel
                            // capacity caps the worst case at 64 frames
                            // before the lagged-frame fast path kicks
                            // in for any other consumer.
                            continue;
                        }
                    };
                    if let Err(e) = writer.write_all(&frame.bytes).await {
                        warn!(
                            error = %e,
                            "wfb_tx stdin write failed; clearing handle until supervisor reinstalls"
                        );
                        *guard = None;
                        continue;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(skipped, "wfb tee consumer lagged; some frames dropped");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    debug!("wfb tee: encoder broadcast closed; tee loop exiting");
                    break;
                }
            }
        }
        Ok(())
    }
}

/// Atomic-write a keypair file at mode 0600. Delegates to the
/// canonical helper in `ados_core::atomic` so the keypair file gets
/// the same crash-safe rename + tempfile-cleanup contract as every
/// other persisted artefact in the agent.
fn write_keypair_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    match ados_core::atomic::write_atomic_secret(path, bytes) {
        Ok(()) => Ok(()),
        Err(ados_core::atomic::AtomicWriteError::Io(e)) => Err(e),
        Err(ados_core::atomic::AtomicWriteError::InvalidMode(m)) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid mode 0o{m:o}"),
        )),
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
            keypair_path: PathBuf::from(DEFAULT_KEYPAIR_PATH),
            advanced: WfbAdvancedOpts::default(),
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
    /// the argv carrying the keypair file path + advanced opts.
    #[tokio::test]
    async fn build_args_after_binding() {
        let m = WfbManager::new(cfg()).expect("ctor");
        assert!(m.build_args().await.expect("ok").is_none());
        m.handle_dongle_event(DongleEvent::Added("wlan0".to_string())).await;
        let args = m.build_args().await.expect("ok").expect("bound");
        assert_eq!(args.interface, "wlan0");
        assert_eq!(args.channel, 161);
        assert_eq!(args.mcs_index, 1);
        assert_eq!(args.advanced.fec_k, 8);
        assert_eq!(args.advanced.fec_n, 12);
        assert!(args.keypair_path.to_string_lossy().contains("wfb-keypair"));
    }

    /// `persist_keypair_file` writes a 64-byte file (32-byte broadcast
    /// + 32-byte public) at mode 0600 and returns the public bytes.
    #[tokio::test]
    async fn persist_keypair_file_writes_concatenated_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kp");
        let mut c = cfg();
        c.keypair_path = path.clone();
        let m = WfbManager::new(c).expect("ctor");
        let public = m.persist_keypair_file().await.expect("write");
        let bytes = std::fs::read(&path).expect("read keypair");
        assert_eq!(bytes.len(), KEY_LEN + PUBLIC_KEY_LEN);
        // Last 32 bytes match the returned public.
        assert_eq!(&bytes[KEY_LEN..], &public[..]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "keypair file must be 0600");
        }
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

    /// `tee_to_wfb_tx` writes the encoded byte stream to the installed
    /// stdin handle in one shot. We hand the manager one half of a
    /// duplex stream as the mock stdin, push two frames through the
    /// broadcast channel, drop the sender, and assert the bytes
    /// arrived in order on the read half.
    #[tokio::test]
    async fn tee_writes_frames_to_installed_stdin() {
        use ados_video::EncodedFrame;
        use tokio::io::AsyncReadExt;

        let m = Arc::new(WfbManager::new(cfg()).expect("ctor"));
        let (mock_stdin, mut reader) = tokio::io::duplex(1024);
        m.set_wfb_tx_stdin(Some(Box::new(mock_stdin))).await;

        let (tx, rx) = tokio::sync::broadcast::channel::<EncodedFrame>(8);

        let m_run = m.clone();
        let tee = tokio::spawn(async move { m_run.tee_to_wfb_tx(rx).await });

        let frame_one = EncodedFrame {
            bytes: vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1e],
            is_keyframe: true,
            pts_ms: 0,
        };
        let frame_two = EncodedFrame {
            bytes: vec![0x00, 0x00, 0x00, 0x01, 0x41, 0xe0],
            is_keyframe: false,
            pts_ms: 33,
        };
        tx.send(frame_one.clone()).expect("send 1");
        tx.send(frame_two.clone()).expect("send 2");
        drop(tx); // close the channel so the tee returns

        // Drain the tee task first, then drop the writer half so the
        // reader observes EOF. Holding the writer alive while reading
        // would race read_to_end against an unfinishable producer.
        tee.await.expect("tee join").expect("tee result");
        m.set_wfb_tx_stdin(None).await;

        let mut buf = Vec::new();
        let read_res = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            reader.read_to_end(&mut buf),
        )
        .await
        .expect("read timeout")
        .expect("read err");
        assert_eq!(read_res, frame_one.bytes.len() + frame_two.bytes.len());

        let mut expected = Vec::new();
        expected.extend_from_slice(&frame_one.bytes);
        expected.extend_from_slice(&frame_two.bytes);
        assert_eq!(buf, expected);
    }

    /// With no stdin handle installed the tee silently drops every
    /// frame and exits cleanly when the channel closes. Models the
    /// pre-supervisor boot window when the encoder is producing but
    /// `wfb_tx` has not been spawned yet.
    #[tokio::test]
    async fn tee_drops_frames_when_no_stdin_attached() {
        use ados_video::EncodedFrame;

        let m = Arc::new(WfbManager::new(cfg()).expect("ctor"));
        let (tx, rx) = tokio::sync::broadcast::channel::<EncodedFrame>(8);

        let m_run = m.clone();
        let tee = tokio::spawn(async move { m_run.tee_to_wfb_tx(rx).await });

        for i in 0..4u8 {
            let f = EncodedFrame {
                bytes: vec![i; 16],
                is_keyframe: i == 0,
                pts_ms: i as u64 * 33,
            };
            tx.send(f).expect("send");
        }
        drop(tx);

        tokio::time::timeout(std::time::Duration::from_secs(2), tee)
            .await
            .expect("join timeout")
            .expect("join")
            .expect("tee result");
    }

    /// Overrun the broadcast channel beyond its capacity and confirm
    /// the consumer logs a Lagged warning, skips the missed frames,
    /// and continues receiving the next live frame instead of
    /// panicking. The capacity is small enough to make Lagged trivial
    /// to reproduce without a flake.
    #[tokio::test]
    async fn tee_handles_lagged_recv_without_panic() {
        use ados_video::EncodedFrame;
        use tokio::io::AsyncReadExt;

        let m = Arc::new(WfbManager::new(cfg()).expect("ctor"));
        let (mock_stdin, mut reader) = tokio::io::duplex(1024);
        m.set_wfb_tx_stdin(Some(Box::new(mock_stdin))).await;

        // Capacity 2 so any third send before a recv lands forces a
        // Lagged event the next time the consumer wakes.
        let (tx, rx) = tokio::sync::broadcast::channel::<EncodedFrame>(2);

        // Pre-fill before the consumer spawns. Three sends with a
        // capacity of 2 means the oldest frame falls off and the next
        // recv returns Lagged(1).
        for i in 0..3u8 {
            tx.send(EncodedFrame {
                bytes: vec![0xAA + i, 0xBB + i],
                is_keyframe: false,
                pts_ms: i as u64,
            })
            .expect("preload send");
        }

        let m_run = m.clone();
        let tee = tokio::spawn(async move { m_run.tee_to_wfb_tx(rx).await });

        // Yield long enough for the consumer to observe the Lagged path
        // and drain the remaining buffered frames.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Now send one more frame post-lag. The consumer must still be
        // alive and write this one to the mock stdin.
        let live = EncodedFrame {
            bytes: vec![0xDE, 0xAD, 0xBE, 0xEF],
            is_keyframe: true,
            pts_ms: 999,
        };
        tx.send(live.clone()).expect("post-lag send");
        drop(tx);

        // Wait for the tee to drain the channel and exit; then close
        // the writer so the reader's `read_to_end` resolves.
        tee.await.expect("tee join").expect("tee result");
        m.set_wfb_tx_stdin(None).await;

        let mut buf = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            reader.read_to_end(&mut buf),
        )
        .await
        .expect("read timeout")
        .expect("read err");

        // The trailing 4 bytes must be the live post-lag frame; the
        // bytes before are the surviving buffered frames after the
        // Lagged drop. We only assert the suffix to keep the test
        // robust against the exact size of the lag window.
        assert!(
            buf.ends_with(&live.bytes),
            "expected live frame at tail, got {:x?}",
            buf
        );
    }

    /// A broken-pipe error on the stdin write clears the installed
    /// handle so the next supervisor restart can plug a fresh one in.
    /// We force the error by closing the read half of the duplex
    /// before the consumer has a chance to write.
    #[tokio::test]
    async fn tee_clears_stdin_on_write_error() {
        use ados_video::EncodedFrame;

        let m = Arc::new(WfbManager::new(cfg()).expect("ctor"));
        // Tiny duplex buffer + immediate read-half drop so the first
        // write_all sees a broken pipe.
        let (mock_stdin, reader) = tokio::io::duplex(8);
        drop(reader);
        m.set_wfb_tx_stdin(Some(Box::new(mock_stdin))).await;

        let (tx, rx) = tokio::sync::broadcast::channel::<EncodedFrame>(4);
        let m_run = m.clone();
        let tee = tokio::spawn(async move { m_run.tee_to_wfb_tx(rx).await });

        // Send a frame larger than the duplex buffer so write_all hits
        // the closed read end deterministically.
        tx.send(EncodedFrame {
            bytes: vec![0u8; 64],
            is_keyframe: false,
            pts_ms: 0,
        })
        .expect("send");
        drop(tx);

        tee.await.expect("tee join").expect("tee result");

        let guard = m.wfb_tx_stdin.lock().await;
        assert!(
            guard.is_none(),
            "stdin handle must be cleared after a broken pipe"
        );
    }
}
