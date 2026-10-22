//! The radio transports behind the governor.
//!
//! Every transport carries the same frames ([`crate::frame`]); the governor
//! has already admitted a frame (channel, power, airtime charged in the
//! ledger) before [`Transport::send`] sees it. `recv` must be cancel-safe:
//! the daemon races it against its timers in a `select!`.

pub mod e22;
pub mod loopback;
pub mod meshtastic;

use std::io;

use ados_lora::airtime::Modem;
use ados_lora::band::ModuleCaps;

/// `lora.transport`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    E22,
    Meshtastic,
    Loopback,
}

impl TransportKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TransportKind::E22 => "e22",
            TransportKind::Meshtastic => "meshtastic",
            TransportKind::Loopback => "loopback",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "e22" => Some(TransportKind::E22),
            "meshtastic" => Some(TransportKind::Meshtastic),
            "loopback" => Some(TransportKind::Loopback),
            _ => None,
        }
    }
}

/// An admitted frame on its way out.
#[derive(Debug, Clone, PartialEq)]
pub struct TxFrame {
    /// Channel centre the governor picked.
    pub channel_mhz: f64,
    /// Conducted output the governor picked, one of the module's steps.
    pub power_dbm: i8,
    /// The encoded [`crate::frame::Frame`].
    pub bytes: Vec<u8>,
}

/// What a transport hands up from the air.
#[derive(Debug, Clone, PartialEq)]
pub enum RxEvent {
    /// A frame in this lane's own format.
    Frame {
        bytes: Vec<u8>,
        rssi_dbm: Option<i16>,
        snr_db: Option<f32>,
    },
    /// A position heard from a Meshtastic node, already decoded.
    MeshPosition {
        node_id: u32,
        lat_deg: f64,
        lon_deg: f64,
        alt_m: Option<f64>,
        battery_pct: Option<u8>,
        rssi_dbm: Option<i16>,
        snr_db: Option<f32>,
    },
}

/// One radio behind the governor.
#[allow(async_fn_in_trait)]
pub trait Transport {
    fn kind(&self) -> TransportKind;

    /// What the fitted radio can tune (read from the module where it can be).
    fn caps(&self) -> &ModuleCaps;

    /// The modem parameters time on air is computed with.
    fn modem(&self) -> Modem;

    /// Multiplier on the computed time on air charged to the ledger: 2 for an
    /// E22 (its air-rate modem parameters are unpublished), 1 otherwise.
    fn airtime_factor(&self) -> u64 {
        1
    }

    /// The radio's own reported transmit utilisation over the last hour, in
    /// percent, when it reports one (a Meshtastic node's `air_util_tx`).
    fn reported_air_util_pct(&self) -> Option<f64> {
        None
    }

    /// The bytes the radio actually puts on the air for this encoded lane
    /// frame (what time on air is charged on), or `None` when the transport
    /// does not radiate it at all.
    fn on_air_len(&self, frame_bytes: &[u8]) -> io::Result<Option<usize>> {
        Ok(Some(frame_bytes.len()))
    }

    /// Bring the radio to the frame's channel and power ahead of the send, so
    /// listen-before-talk can be taken right before the write. `send` must
    /// still work without it.
    async fn prepare(&mut self, _frame: &TxFrame) -> io::Result<()> {
        Ok(())
    }

    /// One ambient RSSI sample on `channel_mhz`, for listen-before-talk.
    /// `Ok(None)` when the radio cannot measure.
    async fn ambient_rssi_dbm(&mut self, channel_mhz: f64) -> io::Result<Option<i16>>;

    /// Put an admitted frame on the air.
    async fn send(&mut self, frame: &TxFrame) -> io::Result<()>;

    /// The next event from the air. Cancel-safe.
    async fn recv(&mut self) -> io::Result<RxEvent>;
}
