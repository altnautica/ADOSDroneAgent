//! The loopback transport: the full governor path with the radio replaced by
//! UDP, for SITL. Frames go to `lora.loopback.peer` as raw frame bytes. From
//! the peer the lane accepts either raw frame bytes, or a JSON datagram:
//!
//! - `{"frame_hex": "<hex>", "rssi_dbm": -97, "snr_db": 6.5}`: a received
//!   frame with link figures;
//! - `{"ambient_rssi_dbm": -70}`: the ambient level listen-before-talk will
//!   measure from now on (`null` restores the quiet floor).
//!
//! `module_band` comes from `lora.loopback.module_band` so the module-band
//! check can be exercised; the modem parameters are the explicit config ones.

use std::io;
use std::net::SocketAddr;

use serde::Deserialize;
use tokio::net::UdpSocket;

use super::{RxEvent, Transport, TransportKind, TxFrame};
use crate::config::LoraConfig;
use ados_lora::airtime::Modem;
use ados_lora::band::ModuleCaps;

/// The ambient level reported when the peer injected none.
pub const QUIET_FLOOR_DBM: i16 = -120;

pub struct Loopback {
    sock: UdpSocket,
    peer: SocketAddr,
    caps: ModuleCaps,
    modem: Modem,
    ambient: i16,
    buf: Vec<u8>,
}

#[derive(Deserialize)]
struct Control {
    frame_hex: Option<String>,
    rssi_dbm: Option<i16>,
    snr_db: Option<f32>,
    #[serde(default, with = "double_option")]
    ambient_rssi_dbm: Option<Option<i16>>,
}

/// Distinguish an absent key from an explicit `null`.
mod double_option {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Option::<T>::deserialize(d).map(Some)
    }
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

impl Loopback {
    pub async fn open(cfg: &LoraConfig) -> io::Result<Self> {
        let caps = ModuleCaps::from_label(&cfg.loopback.module_band).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unknown lora.loopback.module_band {:?}",
                    cfg.loopback.module_band
                ),
            )
        })?;
        let peer: SocketAddr = cfg.loopback.peer.parse().map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("lora.loopback.peer: {e}"),
            )
        })?;
        let sock = UdpSocket::bind(&cfg.loopback.bind).await?;
        Ok(Self {
            sock,
            peer,
            caps,
            modem: cfg.modem(),
            ambient: QUIET_FLOOR_DBM,
            buf: vec![0; 2048],
        })
    }

    /// The bound local address.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.sock.local_addr()
    }
}

impl Transport for Loopback {
    fn kind(&self) -> TransportKind {
        TransportKind::Loopback
    }

    fn caps(&self) -> &ModuleCaps {
        &self.caps
    }

    fn modem(&self) -> Modem {
        self.modem
    }

    async fn ambient_rssi_dbm(&mut self, _channel_mhz: f64) -> io::Result<Option<i16>> {
        Ok(Some(self.ambient))
    }

    async fn send(&mut self, frame: &TxFrame) -> io::Result<()> {
        self.sock.send_to(&frame.bytes, self.peer).await.map(|_| ())
    }

    async fn recv(&mut self) -> io::Result<RxEvent> {
        loop {
            let (n, _) = self.sock.recv_from(&mut self.buf).await?;
            let dgram = &self.buf[..n];
            if dgram.first() != Some(&b'{') {
                return Ok(RxEvent::Frame {
                    bytes: dgram.to_vec(),
                    rssi_dbm: None,
                    snr_db: None,
                });
            }
            let Ok(ctl) = serde_json::from_slice::<Control>(dgram) else {
                tracing::debug!("lora_loopback_bad_control");
                continue;
            };
            if let Some(level) = ctl.ambient_rssi_dbm {
                self.ambient = level.unwrap_or(QUIET_FLOOR_DBM);
            }
            if let Some(bytes) = ctl.frame_hex.as_deref().and_then(unhex) {
                return Ok(RxEvent::Frame {
                    bytes,
                    rssi_dbm: ctl.rssi_dbm,
                    snr_db: ctl.snr_db,
                });
            }
        }
    }
}
