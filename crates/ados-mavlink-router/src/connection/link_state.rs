//! Per-link bookkeeping on [`FcConnection`]: the link generation that keeps a
//! stale write failure from tearing down a healthy replacement link, the
//! inbound-activity clock, the FC device and signing observations, and the PIC
//! gate entry points.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use ados_protocol::ipc::InjectorClaim;
use ados_protocol::shutdown::Shutdown;

use super::framing::STX_V2;
use super::FcConnection;

/// MAVLink 2 incompat flag: the frame carries a signature.
const MAVLINK_IFLAG_SIGNED: u8 = 0x01;

/// Whether `frame` is a signed MAVLink 2 frame.
pub(crate) fn frame_is_signed(frame: &[u8]) -> bool {
    frame.len() > 2 && frame[0] == STX_V2 && frame[2] & MAVLINK_IFLAG_SIGNED != 0
}

/// Whether a canonical serial device is the FC's own USB port (a CDC-ACM
/// node). ArduPilot exempts that link (channel 0) from signing enforcement.
pub(crate) fn device_is_usb(device: &str) -> bool {
    std::path::Path::new(device)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("ttyACM"))
}

/// The canonical device path for a serial port label, or `None` for a network
/// source (`tcp:`/`udp:`/`udpin:` labels) or a path that does not resolve.
pub(crate) async fn canonical_fc_device(port: &str) -> Option<String> {
    if !port.starts_with('/') {
        return None;
    }
    tokio::fs::canonicalize(port)
        .await
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

impl FcConnection {
    /// Bump the link generation. Call with the writer lock held, at the moment
    /// a writer (or deliberately none) is installed for a new link.
    pub(super) fn next_link_gen(&self) -> u64 {
        self.link_gen.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// The current link generation.
    pub(crate) fn link_gen(&self) -> u64 {
        self.link_gen.load(Ordering::Acquire)
    }

    /// Ask the run loop to rebuild the link that was current at `generation`.
    pub(super) fn request_reconnect(&self, generation: u64) {
        self.reconnect.send_replace(Some(generation));
    }

    /// Resolves once a reconnect was requested for link `generation`.
    /// Requests recorded against an older generation are ignored, so a write
    /// failure that raced a read EOF cannot tear down the link that replaced
    /// the one it failed on.
    pub(crate) async fn reconnect_requested(&self, generation: u64) {
        let mut rx = self.reconnect.subscribe();
        loop {
            if *rx.borrow_and_update() == Some(generation) {
                return;
            }
            if rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    /// Stamp the inbound-activity clock.
    pub(super) fn stamp_last_msg(&self) {
        let nanos = u64::try_from(self.epoch.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.last_msg_at.store(nanos, Ordering::Relaxed);
    }

    /// Time since the last inbound frame.
    pub(super) fn last_msg_idle(&self) -> Duration {
        let at = Duration::from_nanos(self.last_msg_at.load(Ordering::Relaxed));
        self.epoch.elapsed().saturating_sub(at)
    }

    /// Record whether the vehicle's latest frame was signed.
    pub(super) fn note_vehicle_frame(&self, frame: &[u8]) {
        self.fc_signed
            .store(frame_is_signed(frame), Ordering::Relaxed);
    }

    /// Whether the vehicle's latest frame carried a MAVLink 2 signature.
    pub fn fc_signed(&self) -> bool {
        self.fc_signed.load(Ordering::Relaxed)
    }

    /// The canonical device path of the open serial FC link, `None` for a
    /// network source or no link.
    pub async fn fc_device(&self) -> Option<String> {
        self.fc_device.lock().await.clone()
    }

    /// Whether the FC is likely dropping this router's own unsigned frames
    /// (stream-rate requests, the parameter sweep, swarm setpoints): it
    /// answers signed, so it holds a key, and the link is a serial link other
    /// than its USB port, where an ArduPilot FC enforces signing. `None` when
    /// there is no open serial link to judge (unmeasured).
    pub async fn agent_commands_unsigned_rejected_risk(&self) -> Option<bool> {
        if !self.transport_open() {
            return None;
        }
        let device = self.fc_device.lock().await;
        let device = device.as_deref()?;
        Some(self.fc_signed() && !device_is_usb(device))
    }

    /// Run the PIC/pairing feed until `cancel`. The gate and the swarm loop
    /// read its samples; no command path reads the files itself.
    pub async fn run_gate_feed(&self, cancel: Shutdown) {
        self.gate_feed.clone().run(cancel).await;
    }

    /// Whether this node gates declared injectors.
    pub fn injector_arbitration_state(&self) -> &'static str {
        self.cfg.injector_arbitration_state()
    }

    /// Whether a declared injector's command is refused right now. Always
    /// false where no gate is built (no local arbiter). A poisoned lock is
    /// recovered: the cache inside is valid, and a panic here would take the
    /// FC path down.
    pub(super) fn injector_refused_now(&self, claim: &InjectorClaim) -> bool {
        match &self.injector_gate {
            Some(gate) => gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .refused(claim),
            None => false,
        }
    }

    /// Whether the PIC arbiter refuses the swarm layer right now. Only a
    /// present, fresh PIC sidecar can refuse it; with none (a drone, which
    /// runs no arbiter) the swarm layer keeps its own gates alone.
    pub fn swarm_refused_by_pic(&self) -> bool {
        pic_refuses_swarm(self.gate_feed.fresh_pic_view(Instant::now()).as_ref())
    }
}

/// The swarm layer's PIC verdict. It is an autonomous injector named `swarm`:
/// refused while anyone else holds the claim. An absent view (no sidecar, or a
/// stale one) does not refuse it, unlike a declared plugin injector, because
/// the swarm layer runs on drones where no arbiter exists.
pub(crate) fn pic_refuses_swarm(view: Option<&ados_hid::pic_view::PicView>) -> bool {
    view.is_some_and(|v| ados_hid::pic_view::injector_refused(Some(v), Some("swarm")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_v2_frame_with_the_signed_flag_is_signed() {
        assert!(frame_is_signed(&[0xFD, 9, 0x01, 0]));
        assert!(!frame_is_signed(&[0xFD, 9, 0x00, 0]));
        // MAVLink 1 has no signing.
        assert!(!frame_is_signed(&[0xFE, 9, 0x01, 0]));
        assert!(!frame_is_signed(&[0xFD]));
    }

    #[test]
    fn only_a_cdc_acm_node_is_the_usb_link() {
        assert!(device_is_usb("/dev/ttyACM0"));
        assert!(!device_is_usb("/dev/ttyAMA0"));
        assert!(!device_is_usb("/dev/ttyUSB0"));
        assert!(!device_is_usb("/dev/ttyS1"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fc_device_canonical_in_extras() {
        // A symlinked device label (the way `/dev/serial0` points at the real
        // UART) resolves to its target, which is what the extra publishes.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("ttyAMA0");
        std::fs::write(&target, b"").unwrap();
        let link = dir.path().join("serial0");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let resolved = canonical_fc_device(link.to_str().unwrap()).await;
        let expected = std::fs::canonicalize(&target).unwrap();
        assert_eq!(resolved, Some(expected.to_string_lossy().into_owned()));
    }

    fn conn() -> std::sync::Arc<FcConnection> {
        let dir = std::env::temp_dir().join("ados-link-state-test-params.json");
        FcConnection::new(
            crate::config::MavlinkConfig::default(),
            std::sync::Arc::new(tokio::sync::Mutex::new(
                crate::state::VehicleState::default(),
            )),
            std::sync::Arc::new(tokio::sync::Mutex::new(
                crate::param_cache::ParamCache::new(dir),
            )),
        )
    }

    #[tokio::test]
    async fn a_signing_fc_on_a_uart_flags_the_unsigned_command_risk() {
        let c = conn();
        // No open link: unmeasured.
        assert_eq!(c.agent_commands_unsigned_rejected_risk().await, None);
        c.connected.store(true, Ordering::Relaxed);
        *c.fc_device.lock().await = Some("/dev/ttyAMA0".into());
        c.note_vehicle_frame(&[0xFD, 9, 0x00, 0]);
        assert_eq!(c.agent_commands_unsigned_rejected_risk().await, Some(false));
        c.note_vehicle_frame(&[0xFD, 9, 0x01, 0]);
        assert_eq!(c.agent_commands_unsigned_rejected_risk().await, Some(true));
        // The FC's own USB port is exempt from enforcement.
        *c.fc_device.lock().await = Some("/dev/ttyACM0".into());
        assert_eq!(c.agent_commands_unsigned_rejected_risk().await, Some(false));
        // A network source has no device to judge.
        *c.fc_device.lock().await = None;
        assert_eq!(c.agent_commands_unsigned_rejected_risk().await, None);
    }

    #[tokio::test]
    async fn arbitration_inactive_on_drone_gates_nothing() {
        let c = conn();
        assert_eq!(c.injector_arbitration_state(), "off");
        let claim = InjectorClaim {
            client_id: "ai-mission".into(),
            ticket: None,
        };
        assert!(!c.injector_refused_now(&claim), "no gate, nothing refused");
        // No sidecar fed: the swarm layer keeps its own gates.
        assert!(!c.swarm_refused_by_pic());
    }
}
