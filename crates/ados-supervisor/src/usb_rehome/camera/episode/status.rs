//! An episode's sidecar slice and its healthy-tick records: the location
//! record and the power-contention diagnostic.

use ados_protocol::logd::emitter::EventEmitter;
use ados_protocol::logd::{Fields, Level, Value};

use super::super::super::topo;
use super::super::os::{CameraOs, LastGood};
use super::super::CAMERA_CONTENTION_KIND;
use super::Episode;

/// One camera's slice of the recovery sidecar.
#[derive(serde::Serialize)]
pub(in crate::usb_rehome::camera) struct EpisodeSnap<'a> {
    camera_id: &'a str,
    camera_usb_recovery_state: &'a str,
    case: &'a str,
    attempts: u32,
    camera_present: bool,
    expected: bool,
    bind_id: &'a str,
    hub: &'a str,
    port: Option<u32>,
    ppps_capable: bool,
    power_contention: bool,
    contention_peer: &'a str,
}

impl Episode {
    /// This camera's slice of the sidecar.
    pub(in crate::usb_rehome::camera) fn snapshot(&self) -> EpisodeSnap<'_> {
        // Power contention is a FAULT worth surfacing only while the camera is
        // actually in a recovery episode. On the healthy/idle path the shared
        // hub is a LATENT topology risk, not a fault — reporting
        // `state:"idle" + power_contention:true` (while the camera streams
        // fine) is a false positive the GCS would badge. It is detected on the
        // healthy tick (the only time the camera's topology can be resolved),
        // kept latent, and exposed only once a recovery is in flight, so the
        // drop can be diagnosed as contention.
        let contention_active = self.power_contention && self.state != "idle";
        EpisodeSnap {
            camera_id: &self.id,
            camera_usb_recovery_state: &self.state,
            case: &self.case,
            attempts: self.machine.attempts(),
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
        }
    }

    /// Detect the power-contention brown-out risk: the camera sharing a hub
    /// (with no per-port power) with the high-draw WFB radio. The radio's TX
    /// spikes starve the camera on an over-subscribed shared hub — the real
    /// cause of the "works for hours then drops, port won't re-enable"
    /// symptom. Recomputed each ready tick (cheap sysfs reads); the diagnostic
    /// is emitted once per false→true transition so the operator sees the
    /// cause + the hardware fix.
    pub(super) async fn check_power_contention<O: CameraOs>(
        &mut self,
        os: &O,
        dev_path: &str,
        events: &EventEmitter,
    ) {
        let Some(cam) = os.video_topo(dev_path).await else {
            return;
        };
        let Some((cam_hub, cam_port)) = topo::hub_and_port(&cam.bind_id) else {
            return;
        };
        let radio = os.radio_topo().await;
        let radio_bind = radio.as_ref().map(|r| r.bind_id.clone());
        let shares_hub = radio
            .as_ref()
            .and_then(|r| topo::hub_and_port(&r.bind_id))
            .map(|(rhub, _)| rhub == cam_hub)
            .unwrap_or(false);
        // Contention only bites when the hub cannot isolate the port: a hub
        // with per-port power gives the camera its own budget.
        let now_contention = shares_hub && !os.ppps_capable(&cam_hub, cam_port);

        if now_contention && !self.power_contention {
            let mut d = Fields::new();
            d.insert("camera".to_string(), Value::from(self.id.as_str()));
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
            events.emit(CAMERA_CONTENTION_KIND, Level::Warn, d);
        }
        self.power_contention = now_contention;
        self.contention_peer = if now_contention {
            radio_bind.unwrap_or_default()
        } else {
            String::new()
        };
    }

    /// Persist where the camera lives (bind id + hub + port + ids) so the
    /// absent case can target it and `auto` expectation arms across reboots.
    pub(super) async fn persist_last_good<O: CameraOs>(&self, os: &O, dev_path: &str) {
        let Some(t) = os.video_topo(dev_path).await else {
            return;
        };
        let Some((hub, port)) = topo::hub_and_port(&t.bind_id) else {
            return;
        };
        let (vid, pid) = os.usb_ids(&t.bind_id).await;
        let lg = LastGood {
            bind_id: t.bind_id,
            hub,
            port,
            vid,
            pid,
            updated_at_unix: os.now_unix(),
        };
        os.write_last_good(self.primary, &self.id, &lg).await;
    }
}
