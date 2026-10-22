//! The shared service state: every receiver, the base machine, RTCM out, the
//! heading and the PPS pairer. Source tasks feed frames in; the sidecar loop
//! and the command socket read it.
//!
//! The first configured receiver is the base and the PPS receiver.
//!
//! Every port open (and, for a UDP receiver, every new peer) re-sends the
//! receiver's whole configuration: the MON-VER poll, the UBX output enables
//! and the base command (or the explicit base-off), all to the RAM layer, so
//! a power-cycled or factory-default receiver comes back configured.

use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::config::{GnssConfig, ReceiverCfg, Source};
use crate::status::{
    BaseOut, GnssStatus, Heading, ReceiverView, RtcmCounter, GNSS_SIDECAR_VERSION,
};
use ados_gnss_stream::base::{Base, BaseMode, BaseState, Capability};
use ados_gnss_stream::receiver::ReceiverState;
use ados_gnss_stream::stream::Frame;
use ados_gnss_stream::{nmea, ubx, valset};

pub type Shared = Arc<Mutex<Service>>;

pub const E_NO_RECEIVER: &str = "E_GNSS_NO_RECEIVER";
pub const E_BAD_REQUEST: &str = "E_GNSS_BAD_REQUEST";

struct Rx {
    cfg: ReceiverCfg,
    state: ReceiverState,
    writer: Option<mpsc::Sender<Vec<u8>>>,
}

/// What an expected CFG-VALSET acknowledgement belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ack {
    /// No acknowledgement (a poll).
    None,
    /// A configuration write the base machine does not track.
    Other,
    /// The base command.
    Base,
}

/// Most acknowledgements kept outstanding; older ones are forgotten.
const MAX_PENDING_ACKS: usize = 8;

pub struct Service {
    rx: Vec<Rx>,
    base: Base,
    /// CFG-VALSET writes to the base receiver awaiting ACK/NAK, in order (a
    /// u-blox receiver acknowledges in the order it received them).
    pending_acks: VecDeque<Ack>,
    rtcm: RtcmCounter,
    heading: Heading,
    heading_enabled: bool,
    pub pairer: crate::pps::Pairer,
}

impl Service {
    /// Build the state. A configured base mode is commanded when the base
    /// receiver's port opens; a hub receiver, which cannot be configured,
    /// reports `failed` at once.
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
            pending_acks: VecDeque::new(),
            rtcm: RtcmCounter::default(),
            heading: Heading::default(),
            heading_enabled: cfg.heading_enabled && cfg.receivers.len() >= 2,
            pairer: crate::pps::Pairer::default(),
        };
        let hub_base = svc.rx.first().is_some_and(|r| r.cfg.source == Source::Hub);
        if cfg.base != BaseMode::Off {
            if hub_base {
                let _ = svc.set_base(cfg.base, now);
            } else {
                svc.base.request(cfg.base);
            }
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

    fn send(&mut self, idx: usize, bytes: Vec<u8>, ack: Ack) {
        let Some(r) = self.rx.get(idx) else {
            return;
        };
        let Some(w) = r.writer.as_ref() else {
            tracing::debug!(id = %r.cfg.id, "gnss_write_no_port");
            return;
        };
        if w.try_send(bytes).is_err() {
            tracing::warn!(id = %r.cfg.id, "gnss_write_dropped");
            return;
        }
        if idx == 0 && ack != Ack::None {
            if self.pending_acks.len() == MAX_PENDING_ACKS {
                self.pending_acks.pop_front();
            }
            self.pending_acks.push_back(ack);
        }
    }

    /// A source opened its port: keep its writer and send the receiver its
    /// whole configuration, the base command included.
    pub fn on_attach(&mut self, idx: usize, writer: mpsc::Sender<Vec<u8>>, now: u64) {
        if idx == 0 {
            self.pending_acks.clear();
        }
        self.configure(idx, writer, now, true);
    }

    /// A UDP receiver's datagrams now come from another address (a bridge
    /// restarted on a new source port): the same receiver, so its outputs are
    /// re-enabled (idempotent) but a base command it already confirmed is not
    /// re-sent, since rewriting CFG-TMODE restarts a running survey-in.
    pub fn on_peer_change(&mut self, idx: usize, writer: mpsc::Sender<Vec<u8>>, now: u64) {
        self.configure(idx, writer, now, false);
    }

    fn configure(&mut self, idx: usize, writer: mpsc::Sender<Vec<u8>>, now: u64, fresh: bool) {
        self.rx[idx].writer = Some(writer);
        let port = self.rx[idx].cfg.port;
        let is_base = idx == 0;
        self.send(idx, valset::poll_mon_ver(), Ack::None);
        let heading = self.heading_enabled && !is_base;
        self.send(idx, valset::nav_outputs(port, is_base, heading), Ack::Other);
        if !is_base {
            return;
        }
        match self.base.mode() {
            BaseMode::Off => self.send(0, valset::off(port), Ack::Other),
            _ if !fresh && self.base.confirmed() => {}
            mode => {
                let _ = self.set_base(mode, now);
            }
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
            self.send(0, bytes, Ack::Base);
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
                let ok = id == ubx::ACK_ACK;
                match self.pending_acks.pop_front() {
                    Some(Ack::Base) => self.base.on_valset_ack(ok),
                    _ if !ok => tracing::debug!("gnss_config_write_refused"),
                    _ => {}
                }
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
