//! The Linux-only edges of camera recovery: the status sidecar, the
//! power-contention check and the last-good location record.

use super::*;

impl CameraUsbRecovery {
    #[cfg(target_os = "linux")]
    pub(super) fn write_sidecar(&self, attempts: u32, cooldown_s: u64) {
        #[derive(serde::Serialize)]
        struct Snap<'a> {
            /// Sidecar schema version (best-effort drift signal for readers).
            version: u16,
            camera_usb_recovery_state: &'a str,
            case: &'a str,
            attempts: u32,
            cooldown_s: u64,
            camera_present: bool,
            expected: bool,
            bind_id: &'a str,
            hub: &'a str,
            port: Option<u32>,
            ppps_capable: bool,
            power_contention: bool,
            contention_peer: &'a str,
            updated_at_unix: u64,
        }
        // Power contention is a FAULT worth surfacing only while the camera is
        // actually in a recovery episode. On the healthy/idle path the shared
        // hub is a LATENT topology risk, not a fault — reporting
        // `state:"idle" + power_contention:true` (while the camera streams fine)
        // is a false positive the GCS would badge. `self.power_contention` is
        // detected on the healthy tick (the only time the camera's topology can
        // be resolved); we keep it latent and expose it on the wire only once a
        // recovery is in flight, so the drop can be diagnosed as contention.
        let contention_active = self.power_contention && self.state != "idle";
        let snap = Snap {
            version: os::CAMERA_USB_RECOVERY_SIDECAR_VERSION,
            camera_usb_recovery_state: &self.state,
            case: &self.case,
            attempts,
            cooldown_s,
            camera_present: self.camera_present,
            expected: self.expected,
            bind_id: &self.bind_id,
            hub: &self.hub,
            port: self.port,
            ppps_capable: self.ppps,
            power_contention: contention_active,
            contention_peer: if contention_active {
                self.contention_peer.as_str()
            } else {
                ""
            },
            updated_at_unix: os::now_unix(),
        };
        let path = std::path::Path::new(os::SIDECAR_PATH);
        if let Err(e) = ados_protocol::sidecar::write_json_atomic(path, &snap, 0o644) {
            tracing::debug!(error = %e, "camera recovery sidecar write failed");
        }
    }

    /// Detect the power-contention brown-out risk: the camera sharing a hub (with
    /// no per-port power) with the high-draw WFB radio. The radio's TX spikes
    /// starve the camera on an over-subscribed shared hub — the real cause of the
    /// "works for hours then drops, port won't re-enable" symptom. Recomputed each
    /// ready tick (cheap sysfs reads); the diagnostic is emitted once per
    /// false→true transition so the operator sees the cause + the hardware fix.
    #[cfg(target_os = "linux")]
    pub(super) async fn check_power_contention(&mut self, primary_path: &str) {
        let Some(cam) = topo::resolve_usb_topo_for_video(primary_path).await else {
            return;
        };
        let Some((cam_hub, cam_port)) = topo::hub_and_port(&cam.bind_id) else {
            return;
        };
        let radio = os::read_radio_topo().await;
        let radio_bind = radio.as_ref().map(|r| r.bind_id.clone());
        let shares_hub = radio
            .as_ref()
            .and_then(|r| topo::hub_and_port(&r.bind_id))
            .map(|(rhub, _)| rhub == cam_hub)
            .unwrap_or(false);
        // Contention only bites when the hub cannot isolate the port: a hub with
        // per-port power gives the camera its own budget, so it is not at risk.
        let now_contention = shares_hub && !topo::ppps_capable(&cam_hub, cam_port);

        if now_contention && !self.power_contention {
            use ados_protocol::logd::{Fields, Level, Value};
            let mut d = Fields::new();
            d.insert(
                "camera_bind_id".to_string(),
                Value::from(cam.bind_id.as_str()),
            );
            d.insert("hub".to_string(), Value::from(cam_hub.as_str()));
            d.insert(
                "radio_bind_id".to_string(),
                Value::from(radio_bind.as_deref().unwrap_or("")),
            );
            d.insert(
                "advice".to_string(),
                Value::from(
                    "camera shares an over-subscribed USB hub with the radio; move \
                     the camera to a separate port or a self-powered hub",
                ),
            );
            self.events.emit(CAMERA_CONTENTION_KIND, Level::Warn, d);
        }
        self.power_contention = now_contention;
        self.contention_peer = if now_contention {
            radio_bind.unwrap_or_default()
        } else {
            String::new()
        };
    }

    /// Persist where the camera lives (bind id + hub + port + ids) so the absent
    /// case can target it and `auto` expectation arms across reboots.
    #[cfg(target_os = "linux")]
    pub(super) async fn persist_last_good(&self, primary_path: &str) {
        let Some(t) = topo::resolve_usb_topo_for_video(primary_path).await else {
            return;
        };
        let Some((hub, port)) = topo::hub_and_port(&t.bind_id) else {
            return;
        };
        let vid = os::read_sysfs_id(&t.bind_id, "idVendor").await;
        let pid = os::read_sysfs_id(&t.bind_id, "idProduct").await;
        let lg = os::LastGood {
            bind_id: t.bind_id,
            hub,
            port,
            vid,
            pid,
            updated_at_unix: os::now_unix(),
        };
        let path = std::path::Path::new(os::LAST_GOOD_PATH);
        if let Err(e) = ados_protocol::sidecar::write_json_atomic(path, &lg, 0o644) {
            tracing::debug!(error = %e, "camera last-good write failed");
        }
    }
}
