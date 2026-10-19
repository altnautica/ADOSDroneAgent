//! The shared service state: every receiver, the base machine, RTCM out, the
//! heading and the PPS pairer. Source tasks feed frames in; the sidecar loop
//! and the command socket read it.
//!
//! The first configured receiver is the base and the PPS receiver.

use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::base::{Base, BaseMode, BaseState, Capability};
use crate::config::{GnssConfig, ReceiverCfg, Source};
use crate::receiver::ReceiverState;
use crate::status::{
    BaseOut, GnssStatus, Heading, ReceiverView, RtcmCounter, GNSS_SIDECAR_VERSION,
};
use crate::stream::Frame;
use crate::{nmea, ubx, valset};

pub type Shared = Arc<Mutex<Service>>;

pub const E_NO_RECEIVER: &str = "E_GNSS_NO_RECEIVER";
pub const E_BAD_REQUEST: &str = "E_GNSS_BAD_REQUEST";

struct Rx {
    cfg: ReceiverCfg,
    state: ReceiverState,
    writer: Option<mpsc::Sender<Vec<u8>>>,
}

pub struct Service {
    rx: Vec<Rx>,
    base: Base,
    rtcm: RtcmCounter,
    heading: Heading,
    heading_enabled: bool,
    pub pairer: crate::pps::Pairer,
}

impl Service {
    /// Build the state and apply the configured base mode.
    pub fn new(cfg: &GnssConfig, now: u64) -> Self {
        let mut svc = Self {
            rx: cfg
                .receivers
                .iter()
                .map(|c| Rx {
                    cfg: c.clone(),
                    state: ReceiverState::new(c.model.clone()),
                    writer: None,
                })
                .collect(),
            base: Base::new(cfg.survey_min_dur_s, cfg.survey_accuracy_m),
            rtcm: RtcmCounter::default(),
            heading: Heading::default(),
            heading_enabled: cfg.heading_enabled && cfg.receivers.len() >= 2,
            pairer: crate::pps::Pairer::default(),
        };
        if cfg.base != BaseMode::Off {
            let _ = svc.set_base(cfg.base, now);
        }
        svc
    }

    pub fn shared(self) -> Shared {
        Arc::new(Mutex::new(self))
    }

    pub fn receiver_id(&self, idx: usize) -> &str {
        &self.rx[idx].cfg.id
    }

    pub fn base_survey_defaults(&self) -> (u32, f64) {
        self.base.defaults()
    }

    fn capability(&self) -> Option<Capability> {
        self.rx.first().map(|r| Capability {
            writable: r.cfg.source != Source::Hub,
            model: r.state.model.clone(),
            port: r.cfg.port,
        })
    }

    fn write(&self, idx: usize, bytes: Vec<u8>) {
        match self.rx.get(idx).and_then(|r| r.writer.as_ref()) {
            Some(w) => {
                if w.try_send(bytes).is_err() {
                    tracing::warn!(id = %self.rx[idx].cfg.id, "gnss_write_dropped");
                }
            }
            None => tracing::debug!(idx, "gnss_write_no_port"),
        }
    }

    /// A source opened its port: keep its writer, ask for the model, and
    /// re-send the base command to the base receiver.
    pub fn on_attach(&mut self, idx: usize, writer: mpsc::Sender<Vec<u8>>, now: u64) {
        self.rx[idx].writer = Some(writer);
        self.write(idx, valset::poll_mon_ver());
        if idx == 0 && self.base.mode() != BaseMode::Off {
            let _ = self.set_base(self.base.mode(), now);
        }
    }

    pub fn on_detach(&mut self, idx: usize) {
        self.rx[idx].writer = None;
    }

    /// Apply a base mode to the base receiver.
    pub fn set_base(&mut self, mode: BaseMode, now: u64) -> Result<(), (&'static str, String)> {
        let Some(cap) = self.capability() else {
            return Err((E_NO_RECEIVER, "no GNSS receiver is configured".to_string()));
        };
        if let Some(bytes) = self.base.apply(mode, &cap, now)? {
            self.write(0, bytes);
        }
        Ok(())
    }

    /// Handle one frame from receiver `idx`. Returns RTCM3 bytes to publish.
    pub fn on_frame(&mut self, idx: usize, frame: Frame, now: u64) -> Option<Vec<u8>> {
        let is_base = idx == 0;
        match frame {
            Frame::Rtcm(bytes) => {
                if is_base && self.base.active() {
                    self.rtcm.record(now, bytes.len());
                    return Some(bytes);
                }
            }
            Frame::Nmea(line) => {
                if let Some(s) = nmea::parse(&line) {
                    self.rx[idx].state.on_nmea(now, s);
                }
            }
            Frame::Ubx { class, id, payload } => self.on_ubx(idx, class, id, &payload, now),
        }
        None
    }

    fn on_ubx(&mut self, idx: usize, class: u8, id: u8, p: &[u8], now: u64) {
        let is_base = idx == 0;
        match (class, id) {
            (ubx::CLS_NAV, ubx::NAV_PVT) => {
                if let Some(pvt) = ubx::nav_pvt(p) {
                    if let (true, Some(utc)) = (is_base, pvt.utc_ns) {
                        self.pairer.on_pvt_time(now, utc);
                    }
                    self.rx[idx].state.on_pvt(now, pvt);
                }
            }
            (ubx::CLS_NAV, ubx::NAV_DOP) => {
                if let Some(h) = ubx::nav_dop_hdop(p) {
                    self.rx[idx].state.on_dop(now, h);
                }
            }
            (ubx::CLS_NAV, ubx::NAV_SVIN) if is_base => {
                if let Some(s) = ubx::nav_svin(p) {
                    self.base.on_svin(&s);
                }
            }
            (ubx::CLS_NAV, ubx::NAV_RELPOSNED) => {
                if let Some(r) = ubx::nav_relposned(p) {
                    self.heading.on_relposned(now, r);
                }
            }
            (ubx::CLS_TIM, ubx::TIM_TP) if is_base => {
                if let Some(utc) = ubx::tim_tp_utc_ns(p) {
                    self.pairer.on_tim_tp(now, utc);
                }
            }
            (ubx::CLS_MON, ubx::MON_VER) => {
                if let Some(m) = ubx::mon_ver_model(p) {
                    if is_base {
                        self.base.on_model(&m);
                    }
                    self.rx[idx].state.model = Some(m);
                }
            }
            (ubx::CLS_ACK, ubx::ACK_ACK | ubx::ACK_NAK)
                if is_base && ubx::ack_target(p) == Some((ubx::CLS_CFG, ubx::CFG_VALSET)) =>
            {
                self.base.on_valset_ack(id == ubx::ACK_ACK);
            }
            _ => {}
        }
    }

    /// Advance timeouts.
    pub fn tick(&mut self, now: u64) {
        self.base.tick(now);
    }

    pub fn base_state(&self) -> BaseState {
        self.base.state()
    }

    pub fn base_reason(&self) -> Option<&str> {
        self.base.reason()
    }

    /// The `base` object.
    pub fn base_out(&mut self, now: u64, targets: u32) -> BaseOut {
        BaseOut {
            base: self.base.view(),
            rtcm_out: self.rtcm.view(now, targets),
        }
    }

    /// The sidecar body; `targets` is the linked-peer count.
    pub fn status(&mut self, now: u64, targets: u32) -> GnssStatus {
        let pps_locked = self.pairer.locked(now);
        let receivers = self
            .rx
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let s = r.state.solution(now);
                ReceiverView {
                    id: r.cfg.id.clone(),
                    source: r.cfg.source.label(),
                    model: r.state.model.clone(),
                    fix: s.fix,
                    sats: s.sats,
                    hdop: s.hdop,
                    lat_deg: s.lat_deg,
                    lon_deg: s.lon_deg,
                    h_ellipsoid_m: s.h_ellipsoid_m,
                    h_msl_m: s.h_msl_m,
                    rtk: s.fix.rtk(),
                    pps_locked: i == 0 && pps_locked,
                }
            })
            .collect();
        GnssStatus {
            version: GNSS_SIDECAR_VERSION,
            receivers,
            base: self.base_out(now, targets),
            heading: self.heading.view(now, self.heading_enabled),
        }
    }
}

#[cfg(test)]
mod tests;
