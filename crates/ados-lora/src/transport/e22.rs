//! Ebyte E22-900T22D / E22-400T22D UART module (manual v1.3).
//!
//! With M0/M1 on GPIO lines the transport puts the module in configuration
//! mode at start, reads the product information (`C1 80 07`), and writes
//! broadcast address `FFFF`, UART 9600 8N1, air rate 2.4k, ambient-noise RSSI
//! on, the RSSI byte on and fixed-point transmission on. Every frame is then
//! sent as `ADDH ADDL CHAN payload` (target `FFFF`), so the channel changes per
//! frame without a mode switch; a power change rewrites REG1 with the
//! temporary-write command `C2`, which needs a switch to configuration mode
//! and back.
//!
//! With strapped M0/M1 (both offsets unset) configuration mode is out of
//! reach: the module must already be set up as above, and since its output
//! step cannot be changed or read the transport only offers 22 dBm, the
//! highest step, so the governor never assumes a lower output than the module
//! may be running.
//!
//! Ebyte does not publish the layout of the product information bytes, so the
//! frequency class comes from `lora.e22.module_band`; the bytes are logged.

mod pins;
mod proto;
#[cfg(test)]
mod tests;

use std::io;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_serial::{SerialPortBuilderExt, SerialStream};

pub use pins::{GpioModePins, Mode, ModePins};

use crate::airtime::Modem;
use crate::band::ModuleCaps;
use crate::config::LoraConfig;
use crate::transport::{RxEvent, Transport, TransportKind, TxFrame};
use proto::{RxBuf, Scan};

/// Configuration mode only accepts 9600 8N1 (manual section 7.1).
const CONFIG_BAUD: u32 = 9600;
/// The module switches mode within 1 ms of AUX going high and works in the new
/// mode 2 ms after; without an AUX line the transport waits this long.
const MODE_SETTLE: Duration = Duration::from_millis(50);
/// Longest wait for a register reply.
const REPLY_TIMEOUT: Duration = Duration::from_secs(1);
/// Longest wait for the ambient-noise reply.
const AMBIENT_TIMEOUT: Duration = Duration::from_millis(100);
/// The lowest output step, written at start until a frame asks for more.
const START_POWER_DBM: i8 = 10;
/// The only output assumed for a module with strapped mode lines.
const STRAPPED_POWER_DBM: i8 = 22;

/// An E22 module on a UART.
pub struct E22<S = SerialStream, P = GpioModePins> {
    io: S,
    /// `None` when M0/M1 are strapped.
    pins: Option<P>,
    caps: ModuleCaps,
    rx: RxBuf,
    power_dbm: i8,
    lbt: bool,
    /// The REG2 channel the module listens on, when known.
    rx_chan: Option<u8>,
    pid: Option<[u8; 7]>,
}

impl E22 {
    /// Open the module named by `lora.e22`.
    pub async fn open(cfg: &LoraConfig) -> io::Result<Self> {
        let e = &cfg.e22;
        let pins = match (e.m0_gpio, e.m1_gpio) {
            (Some(m0), Some(m1)) => Some(GpioModePins::open(&e.gpio_chip, m0, m1)?),
            (None, None) => None,
            _ => return Err(invalid("lora.e22.m0_gpio and m1_gpio must be set together")),
        };
        let io = tokio_serial::new(&e.port, e.baud)
            .open_native_async()
            .map_err(|err| io::Error::other(format!("open {}: {err}", e.port)))?;
        Self::with_io(io, pins, cfg).await
    }
}

impl<S, P> E22<S, P>
where
    S: AsyncRead + AsyncWrite + Unpin,
    P: ModePins,
{
    /// Bring up a module already connected on `io`.
    pub async fn with_io(io: S, pins: Option<P>, cfg: &LoraConfig) -> io::Result<Self> {
        if cfg.air_rate != "2.4k" {
            return Err(invalid(format!(
                "lora.air_rate {:?} is out of plan on the E22 (only 2.4k)",
                cfg.air_rate
            )));
        }
        let mut caps = match cfg.e22.module_band.as_str() {
            "850-930" => ModuleCaps::e22_900(),
            "410-493" => ModuleCaps::e22_400(),
            other => {
                return Err(invalid(format!(
                    "lora.e22.module_band {other:?} is not 850-930 or 410-493"
                )))
            }
        };
        let chan = match cfg.freq_mhz {
            Some(mhz) => Some(proto::channel_index(&caps, mhz).ok_or_else(|| {
                invalid(format!(
                    "lora.freq_mhz {mhz} is not a channel of the module"
                ))
            })?),
            None => None,
        };
        if pins.is_some() && cfg.e22.baud != CONFIG_BAUD {
            return Err(invalid(
                "lora.e22.baud must be 9600 when the mode lines are wired",
            ));
        }
        let strapped = pins.is_none();
        if strapped {
            caps.power_steps_dbm = vec![STRAPPED_POWER_DBM];
        }
        let mut me = Self {
            io,
            pins,
            caps,
            rx: RxBuf::default(),
            power_dbm: if strapped {
                STRAPPED_POWER_DBM
            } else {
                START_POWER_DBM
            },
            lbt: false,
            rx_chan: chan,
            pid: None,
        };
        if strapped {
            tracing::info!(band = %me.caps.band, "E22 mode lines strapped; module configuration not checked");
            return Ok(me);
        }
        me.set_mode(Mode::Config).await?;
        let configured = me.configure(chan).await;
        me.set_mode(Mode::Normal).await?;
        configured?;
        Ok(me)
    }

    /// The product information bytes read at start (`None` when strapped).
    pub fn pid(&self) -> Option<[u8; 7]> {
        self.pid
    }

    /// Turn the module's own listen-before-talk (REG3 bit 4) on or off.
    pub async fn set_lbt(&mut self, on: bool) -> io::Result<()> {
        if on == self.lbt {
            return Ok(());
        }
        self.write_temp(proto::REG_REG3, proto::reg3(on)).await?;
        self.lbt = on;
        Ok(())
    }

    /// Retune the receive channel (REG2) to `mhz`.
    pub async fn set_rx_channel(&mut self, mhz: f64) -> io::Result<()> {
        let chan = self.channel(mhz)?;
        if self.rx_chan == Some(chan) {
            return Ok(());
        }
        self.write_temp(proto::REG_REG2, chan).await?;
        self.rx_chan = Some(chan);
        Ok(())
    }

    /// Read the PID and write the full configuration. Configuration mode.
    async fn configure(&mut self, chan: Option<u8>) -> io::Result<()> {
        let cmd = [proto::CMD_READ, proto::REG_PID, proto::PID_LEN];
        let pid = self
            .exchange(
                &cmd,
                proto::reply_header(proto::REG_PID, proto::PID_LEN),
                REPLY_TIMEOUT,
            )
            .await?;
        let pid: [u8; 7] = pid
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "short E22 PID reply"))?;
        tracing::info!(pid = %hex(&pid), band = %self.caps.band, "E22 module found");
        self.pid = Some(pid);
        let reg1 = proto::reg1(self.power_dbm).expect("start power is a module step");
        let [addh, addl] = proto::BROADCAST_ADDR;
        let head = [addh, addl, proto::NETID, proto::REG0_9600_8N1_2K4, reg1];
        self.write(proto::CMD_WRITE_SAVE, proto::REG_ADDH, &head)
            .await?;
        if let Some(chan) = chan {
            self.write(proto::CMD_WRITE_SAVE, proto::REG_REG2, &[chan])
                .await?;
        }
        self.write(
            proto::CMD_WRITE_SAVE,
            proto::REG_REG3,
            &[proto::reg3(self.lbt)],
        )
        .await
    }

    /// One temporary register write, switching to configuration mode and back.
    async fn write_temp(&mut self, addr: u8, value: u8) -> io::Result<()> {
        if self.pins.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "E22 mode lines are strapped; registers cannot be written",
            ));
        }
        self.set_mode(Mode::Config).await?;
        let written = self.write(proto::CMD_WRITE_TEMP, addr, &[value]).await;
        self.set_mode(Mode::Normal).await?;
        written
    }

    /// Write `data` at `addr` and check the module echoes it.
    async fn write(&mut self, cmd: u8, addr: u8, data: &[u8]) -> io::Result<()> {
        let header = proto::reply_header(addr, data.len() as u8);
        let echo = self
            .exchange(&proto::write_cmd(cmd, addr, data), header, REPLY_TIMEOUT)
            .await?;
        if echo != data {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "E22 register {addr:#04x} reads back {} after writing {}",
                    hex(&echo),
                    hex(data)
                ),
            ));
        }
        Ok(())
    }

    /// Send `cmd` and wait for the reply starting with `header`.
    async fn exchange(
        &mut self,
        cmd: &[u8],
        header: [u8; 3],
        wait: Duration,
    ) -> io::Result<Vec<u8>> {
        self.io.write_all(cmd).await?;
        self.io.flush().await?;
        tokio::time::timeout(wait, self.reply(header, cmd))
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("no E22 reply to {}", hex(cmd)),
                )
            })?
    }

    /// Read until the reply starting with `header` (or a wrong-format reply).
    async fn reply(&mut self, header: [u8; 3], cmd: &[u8]) -> io::Result<Vec<u8>> {
        loop {
            match self.rx.scan(Some(header)) {
                Scan::Reply(data) => return Ok(data),
                Scan::Wrong => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("E22 rejected command {}", hex(cmd)),
                    ))
                }
                Scan::Progress => {}
                Scan::NeedMore => self.fill().await?,
            }
        }
    }

    /// Read whatever the UART has into the receive buffer. Cancel-safe: bytes
    /// are only appended after the read completes.
    async fn fill(&mut self) -> io::Result<()> {
        let mut tmp = [0u8; 256];
        let n = self.io.read(&mut tmp).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "E22 UART closed",
            ));
        }
        self.rx.buf.extend_from_slice(&tmp[..n]);
        Ok(())
    }

    async fn set_mode(&mut self, mode: Mode) -> io::Result<()> {
        if let Some(pins) = self.pins.as_mut() {
            pins.set_mode(mode)?;
            tokio::time::sleep(MODE_SETTLE).await;
        }
        Ok(())
    }

    fn channel(&self, mhz: f64) -> io::Result<u8> {
        proto::channel_index(&self.caps, mhz).ok_or_else(|| {
            invalid(format!(
                "{mhz} MHz is not a channel of the {} module",
                self.caps.band
            ))
        })
    }
}

impl<S, P> Transport for E22<S, P>
where
    S: AsyncRead + AsyncWrite + Unpin,
    P: ModePins,
{
    fn kind(&self) -> TransportKind {
        TransportKind::E22
    }

    fn caps(&self) -> &ModuleCaps {
        &self.caps
    }

    /// The 2.4k air rate is charged as SF9 / 125 kHz / 4-5; Ebyte does not
    /// publish the modem parameters behind it.
    fn modem(&self) -> Modem {
        Modem::SF9_BW125
    }

    fn airtime_factor(&self) -> u64 {
        2
    }

    /// The module measures on its receive channel, so a sample on another
    /// channel retunes REG2 first; with strapped mode lines only the configured
    /// channel can be measured.
    async fn ambient_rssi_dbm(&mut self, channel_mhz: f64) -> io::Result<Option<i16>> {
        let chan = self.channel(channel_mhz)?;
        if self.rx_chan != Some(chan) {
            if self.pins.is_none() {
                return Ok(None);
            }
            self.set_rx_channel(channel_mhz).await?;
        }
        let v = self
            .exchange(&proto::AMBIENT_QUERY, proto::AMBIENT_REPLY, AMBIENT_TIMEOUT)
            .await?;
        Ok(v.first().map(|v| proto::rssi_dbm(*v)))
    }

    async fn send(&mut self, frame: &TxFrame) -> io::Result<()> {
        let chan = self.channel(frame.channel_mhz)?;
        if frame.power_dbm != self.power_dbm {
            if self.pins.is_none() {
                return Err(invalid(format!(
                    "{} dBm requested; a strapped E22 is only driven at {STRAPPED_POWER_DBM} dBm",
                    frame.power_dbm
                )));
            }
            let reg1 = proto::reg1(frame.power_dbm).ok_or_else(|| {
                invalid(format!("{} dBm is not an E22 output step", frame.power_dbm))
            })?;
            self.write_temp(proto::REG_REG1, reg1).await?;
            self.power_dbm = frame.power_dbm;
        }
        let [addh, addl] = proto::BROADCAST_ADDR;
        let mut out = Vec::with_capacity(3 + frame.bytes.len());
        out.extend_from_slice(&[addh, addl, chan]);
        out.extend_from_slice(&frame.bytes);
        self.io.write_all(&out).await?;
        self.io.flush().await
    }

    async fn recv(&mut self) -> io::Result<RxEvent> {
        loop {
            if let Some(ev) = self.rx.pending.pop_front() {
                return Ok(ev);
            }
            if self.rx.scan(None) == Scan::NeedMore {
                self.fill().await?;
            }
        }
    }
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}
