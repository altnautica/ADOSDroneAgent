//! One radio and its governor: the admit -> listen-before-talk -> charge ->
//! send path, and the receive side's beacon list.

use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use tokio::time::Instant;

use serde_json::{json, Value};

use crate::frame::{Beacon, Frame, Kind, Status};
use crate::transport::e22::E22;
use crate::transport::loopback::Loopback;
use crate::transport::meshtastic::Meshtastic;
use crate::transport::{RxEvent, Transport, TransportKind, TxFrame};
use ados_lora::band::ModuleCaps;
use ados_lora::governor::{lbt, Governor, Refusal};
use ados_lora::ledger::Now;
use ados_lora::regions::LbtRule;

/// The configured transport.
pub enum AnyTransport {
    E22(Box<E22>),
    Meshtastic(Box<Meshtastic>),
    Loopback(Loopback),
}

macro_rules! each {
    ($self:ident, $t:ident => $e:expr) => {
        match $self {
            AnyTransport::E22($t) => $e,
            AnyTransport::Meshtastic($t) => $e,
            AnyTransport::Loopback($t) => $e,
        }
    };
}

impl Transport for AnyTransport {
    fn kind(&self) -> TransportKind {
        each!(self, t => t.kind())
    }
    fn caps(&self) -> &ModuleCaps {
        each!(self, t => t.caps())
    }
    fn modem(&self) -> ados_lora::airtime::Modem {
        each!(self, t => t.modem())
    }
    fn airtime_factor(&self) -> u64 {
        each!(self, t => t.airtime_factor())
    }
    fn reported_air_util_pct(&self) -> Option<f64> {
        each!(self, t => t.reported_air_util_pct())
    }
    fn on_air_len(&self, frame_bytes: &[u8]) -> io::Result<Option<usize>> {
        each!(self, t => t.on_air_len(frame_bytes))
    }
    async fn prepare(&mut self, frame: &TxFrame) -> io::Result<()> {
        each!(self, t => t.prepare(frame).await)
    }
    async fn ambient_rssi_dbm(&mut self, channel_mhz: f64) -> io::Result<Option<i16>> {
        each!(self, t => t.ambient_rssi_dbm(channel_mhz).await)
    }
    async fn send(&mut self, frame: &TxFrame) -> io::Result<()> {
        each!(self, t => t.send(frame).await)
    }
    async fn recv(&mut self) -> io::Result<RxEvent> {
        each!(self, t => t.recv().await)
    }
}

/// Beacons kept for `GET /api/v1/lora/beacons`.
pub const BEACON_CAP: usize = 100;

/// A radio, its governor and what it heard.
pub struct Lane {
    pub transport: AnyTransport,
    pub governor: Governor,
    pub node_id: u16,
    pub rx_frames: u64,
    pub rx_crc_errors: u64,
    /// The last node heard and how well, for the next status report.
    pub last_heard: Option<(u16, i16, f32)>,
}

/// Outcome of one transmit attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum TxOutcome {
    Sent,
    Refused(Refusal),
    /// The transport does not radiate this frame (a Meshtastic bridge drops
    /// status frames and beacons without a fix); nothing was charged.
    NotRadiated,
}

/// A small xorshift for the listen-before-talk back-off.
fn jitter() -> u32 {
    let mut x = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
        | 1;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x as u32
}

impl Lane {
    /// The next event from the radio. Cancel-safe.
    pub async fn transport_recv(&mut self) -> io::Result<RxEvent> {
        self.transport.recv().await
    }

    /// The longest beacon frame this node sends (a fix, altitude, battery and
    /// a full-length mode).
    pub fn max_beacon(&self) -> Frame {
        let beacon = Beacon {
            position: Some((-89.999_999_9, -179.999_999_9)),
            alt_m: Some(-32_000.0),
            battery_pct: Some(100),
            mode: "M".repeat(crate::frame::MAX_MODE),
        };
        Frame {
            node_id: self.node_id,
            kind: Kind::Beacon,
            payload: beacon.to_payload(),
        }
    }

    /// Bytes the radio radiates for the longest beacon.
    pub fn max_beacon_on_air(&self) -> io::Result<usize> {
        let bytes = self
            .max_beacon()
            .encode()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        Ok(self.transport.on_air_len(&bytes)?.unwrap_or(0))
    }

    /// Charged airtime of one beacon cycle: the longest beacon plus the share
    /// of a status frame (one every [`super::STATUS_EVERY`] beacons).
    pub fn cycle_charge_us(&self) -> io::Result<u64> {
        let status = Frame {
            node_id: self.node_id,
            kind: Kind::Status,
            payload: Status {
                about_node: 0,
                rssi_dbm: 0,
                snr_db: 0.0,
            }
            .to_payload(),
        }
        .encode()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let charge = |len: Option<usize>| len.map_or(0, |l| self.governor.charge_us(l));
        let beacon = charge(Some(self.max_beacon_on_air()?).filter(|l| *l > 0));
        let status = charge(self.transport.on_air_len(&status)?);
        Ok(beacon + status.div_ceil(super::STATUS_EVERY))
    }

    /// One listen interval on `channel`: `Some(last sample time)` when every
    /// sample was at or below the threshold. A radio that returns no sample
    /// cannot prove the channel clear, so that counts as busy.
    async fn listen_once(&mut self, rule: &LbtRule, channel: f64) -> io::Result<Option<Instant>> {
        let listen = Duration::from_micros(u64::from(rule.listen_us));
        let start = Instant::now();
        let mut samples = Vec::new();
        loop {
            match self.transport.ambient_rssi_dbm(channel).await? {
                Some(v) => samples.push(v),
                None => {
                    samples.clear();
                    break;
                }
            }
            if start.elapsed() >= listen {
                break;
            }
            tokio::time::sleep(listen / 5).await;
        }
        let at = Instant::now();
        let clear = lbt::clear(&samples, rule);
        self.governor.note_lbt(samples.last().copied(), !clear);
        Ok(clear.then_some(at))
    }

    /// Listen before talk with back-off, up to [`lbt::MAX_ATTEMPTS`]; the
    /// time of the last clear sample, or `None` (busy).
    async fn listen(&mut self, rule: &LbtRule, channel: f64) -> io::Result<Option<Instant>> {
        for _ in 0..lbt::MAX_ATTEMPTS {
            if let Some(at) = self.listen_once(rule, channel).await? {
                return Ok(Some(at));
            }
            tokio::time::sleep(Duration::from_micros(lbt::backoff_us(rule, jitter()))).await;
        }
        Ok(None)
    }

    /// Put `frame` on the air through the governor.
    ///
    /// Order: admit on the bytes the radio actually radiates, bring the radio
    /// to the granted channel and power, listen (plans with `lbt`), charge the
    /// ledger (fsync'd, so a crash over-counts), then, if the gap since the
    /// last clear sample has grown past `gap_max_us` while charging, listen
    /// once more right before the write. A busy re-listen refuses the frame
    /// with its charge kept (over-counting, never under-counting).
    pub async fn transmit(
        &mut self,
        frame: &Frame,
        now: impl Fn() -> Now,
    ) -> io::Result<TxOutcome> {
        let bytes = frame
            .encode()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let Some(on_air) = self.transport.on_air_len(&bytes)? else {
            return Ok(TxOutcome::NotRadiated);
        };
        self.governor
            .set_reported_air_util(self.transport.reported_air_util_pct());
        let grant = match self.governor.admit(on_air, None, now()) {
            Ok(g) => g,
            Err(r) => return Ok(TxOutcome::Refused(r)),
        };
        let tx = TxFrame {
            channel_mhz: grant.channel_mhz,
            power_dbm: grant.power_dbm,
            bytes,
        };
        self.transport.prepare(&tx).await?;
        let lbt = self.governor.plan().and_then(|p| p.lbt);
        let mut clear_at = None;
        if let Some(rule) = lbt {
            clear_at = self.listen(&rule, grant.channel_mhz).await?;
            if clear_at.is_none() {
                self.governor.refuse(Refusal::LbtBusy, now());
                return Ok(TxOutcome::Refused(Refusal::LbtBusy));
            }
        }
        self.governor.commit(&grant, now())?;
        if let (Some(rule), Some(at)) = (lbt, clear_at) {
            let gap_max = Duration::from_micros(u64::from(rule.gap_max_us));
            if at.elapsed() > gap_max && self.listen_once(&rule, grant.channel_mhz).await?.is_none()
            {
                self.governor.refuse(Refusal::LbtBusy, now());
                return Ok(TxOutcome::Refused(Refusal::LbtBusy));
            }
        }
        self.transport.send(&tx).await?;
        self.governor.note_sent();
        Ok(TxOutcome::Sent)
    }

    /// Handle one received event: returns the beacon-list entry it produced.
    pub fn on_rx(&mut self, ev: RxEvent, now: Now) -> Option<Value> {
        match ev {
            RxEvent::Frame {
                bytes,
                rssi_dbm,
                snr_db,
            } => {
                let frame = match Frame::decode(&bytes) {
                    Ok((f, _)) => f,
                    Err(crate::frame::FrameError::Crc) => {
                        self.rx_crc_errors += 1;
                        return None;
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "lora_rx_undecodable");
                        return None;
                    }
                };
                self.rx_frames += 1;
                match frame.kind {
                    Kind::Beacon => {
                        let b = Beacon::from_payload(&frame.payload).ok()?;
                        self.last_heard = Some((
                            frame.node_id,
                            rssi_dbm.unwrap_or(i16::MIN),
                            snr_db.unwrap_or(f32::NAN),
                        ));
                        Some(beacon_entry(
                            u32::from(frame.node_id),
                            "beacon",
                            b.position,
                            b.alt_m,
                            b.battery_pct,
                            Some(b.mode),
                            rssi_dbm,
                            snr_db,
                            now.utc_ms,
                        ))
                    }
                    Kind::Status => {
                        let s = Status::from_payload(&frame.payload).ok()?;
                        if s.about_node == self.node_id {
                            self.governor.note_peer_report(frame.node_id, s.snr_db, now);
                        }
                        None
                    }
                    Kind::Text => None,
                }
            }
            RxEvent::MeshPosition {
                node_id,
                lat_deg,
                lon_deg,
                alt_m,
                battery_pct,
                rssi_dbm,
                snr_db,
            } => {
                self.rx_frames += 1;
                Some(beacon_entry(
                    node_id,
                    "beacon",
                    Some((lat_deg, lon_deg)),
                    alt_m,
                    battery_pct,
                    None,
                    rssi_dbm,
                    snr_db,
                    now.utc_ms,
                ))
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn beacon_entry(
    node_id: u32,
    kind: &str,
    position: Option<(f64, f64)>,
    alt_m: Option<f64>,
    battery_pct: Option<u8>,
    mode: Option<String>,
    rssi_dbm: Option<i16>,
    snr_db: Option<f32>,
    at_ms: i64,
) -> Value {
    json!({
        "node_id": node_id,
        "kind": kind,
        "lat_deg": position.map(|p| p.0),
        "lon_deg": position.map(|p| p.1),
        "alt_m": alt_m,
        "battery_pct": battery_pct,
        "mode": mode,
        "rssi_dbm": rssi_dbm,
        "snr_db": snr_db,
        "received_at_ms": at_ms,
    })
}

/// Push `entry` to the front of the bounded beacon list.
pub fn remember(list: &mut VecDeque<Value>, entry: Value) {
    list.push_front(entry);
    list.truncate(BEACON_CAP);
}
