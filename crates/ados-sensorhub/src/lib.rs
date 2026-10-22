#![doc = include_str!("../README.md")]
#![no_std]
#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod cobs;
mod codec;
pub mod crc;
pub mod packet;
mod wire;

pub use packet::*;

/// Largest payload (`len`) the protocol allows.
pub const MAX_PAYLOAD: usize = 1024;
/// Raw frame overhead: type (1) + len (2) + crc (2).
pub const RAW_OVERHEAD: usize = 5;
/// Largest raw (pre-COBS) frame.
pub const MAX_RAW: usize = MAX_PAYLOAD + RAW_OVERHEAD;
/// Largest wire frame including the `0x00` delimiter.
pub const MAX_FRAME: usize = cobs::max_encoded_len(MAX_RAW) + 1;

/// Framing and parsing errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The output buffer cannot hold the result.
    BufferTooSmall,
    /// Malformed COBS block.
    Cobs,
    /// `len` above [`MAX_PAYLOAD`], or decoded length ≠ `len + 5`.
    Length,
    /// CRC mismatch.
    Crc,
    /// The payload size does not fit the packet type.
    PayloadSize {
        /// The packet type byte.
        ty: u8,
    },
    /// `ImuBatch.n` outside 1..=64.
    ImuCount,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BufferTooSmall => f.write_str("buffer too small"),
            Self::Cobs => f.write_str("malformed COBS block"),
            Self::Length => f.write_str("frame length mismatch"),
            Self::Crc => f.write_str("CRC mismatch"),
            Self::PayloadSize { ty } => write!(f, "bad payload size for type 0x{ty:02x}"),
            Self::ImuCount => f.write_str("IMU batch sample count outside 1..=64"),
        }
    }
}

impl core::error::Error for Error {}

/// Write the raw frame (type, len, payload, crc) of `pkt` into `out`;
/// returns its length.
pub fn encode_raw(pkt: &Packet<'_>, out: &mut [u8]) -> Result<usize, Error> {
    let len = pkt.payload_len()?;
    let total = len + RAW_OVERHEAD;
    if out.len() < total {
        return Err(Error::BufferTooSmall);
    }
    out[0] = pkt.ty();
    // payload_len() bounds len to MAX_PAYLOAD.
    out[1..3].copy_from_slice(&(len as u16).to_le_bytes());
    pkt.write_payload(&mut out[3..3 + len]);
    let crc = crc::crc16(&out[..3 + len]);
    out[3 + len..total].copy_from_slice(&crc.to_le_bytes());
    Ok(total)
}

/// Encode `pkt` as one wire frame (COBS + `0x00`) into `out`, using
/// `scratch` (≥ the raw length, [`MAX_RAW`] always suffices) for the raw
/// frame. Returns the wire length including the delimiter.
pub fn encode_frame(pkt: &Packet<'_>, scratch: &mut [u8], out: &mut [u8]) -> Result<usize, Error> {
    let raw = encode_raw(pkt, scratch)?;
    let n = cobs::encode(&scratch[..raw], out)?;
    let slot = out.get_mut(n).ok_or(Error::BufferTooSmall)?;
    *slot = 0;
    Ok(n + 1)
}

/// Parse a raw frame (type, len, payload, crc).
pub fn decode_raw(raw: &[u8]) -> Result<Packet<'_>, Error> {
    if raw.len() < RAW_OVERHEAD {
        return Err(Error::Length);
    }
    let len = usize::from(u16::from_le_bytes([raw[1], raw[2]]));
    if len > MAX_PAYLOAD || raw.len() != len + RAW_OVERHEAD {
        return Err(Error::Length);
    }
    let crc = u16::from_le_bytes([raw[3 + len], raw[4 + len]]);
    if crc::crc16(&raw[..3 + len]) != crc {
        return Err(Error::Crc);
    }
    Packet::parse(raw[0], &raw[3..3 + len])
}

/// Decode one COBS block (delimiter stripped) into `buf` and parse it.
pub fn decode_frame<'b>(block: &[u8], buf: &'b mut [u8]) -> Result<Packet<'b>, Error> {
    let n = cobs::decode(block, buf)?;
    decode_raw(&buf[..n])
}

/// Splits a byte stream into COBS blocks on `0x00`. A block longer than `N`
/// is discarded up to the next delimiter (resync).
pub struct Deframer<const N: usize> {
    buf: [u8; N],
    len: usize,
    overflow: bool,
}

impl<const N: usize> Default for Deframer<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Outcome of pushing one byte into a [`Deframer`].
#[derive(Debug, PartialEq, Eq)]
pub enum Push<'a> {
    /// Mid-frame.
    Pending,
    /// A complete COBS block (delimiter stripped), ready for
    /// [`decode_frame`].
    Block(&'a [u8]),
    /// A block exceeded the buffer and was dropped.
    Overflow,
}

impl<const N: usize> Deframer<N> {
    /// An empty deframer with an `N`-byte block buffer. Size `N` to the
    /// largest COBS block you expect: [`MAX_FRAME`] holds any valid frame.
    pub const fn new() -> Self {
        Self {
            buf: [0; N],
            len: 0,
            overflow: false,
        }
    }

    /// Feed one byte from the stream. On a `0x00` delimiter this returns the
    /// buffered block as [`Push::Block`], or [`Push::Overflow`] if the block
    /// outgrew the buffer; an empty block (two delimiters in a row) returns
    /// [`Push::Pending`]. Every other byte is buffered and returns
    /// [`Push::Pending`]. The returned block borrows the deframer until the
    /// next push.
    pub fn push(&mut self, b: u8) -> Push<'_> {
        if b == 0 {
            let (len, overflow) = (self.len, self.overflow);
            self.len = 0;
            self.overflow = false;
            return match (overflow, len) {
                (true, _) => Push::Overflow,
                (false, 0) => Push::Pending,
                (false, n) => Push::Block(&self.buf[..n]),
            };
        }
        if self.len == N {
            self.overflow = true;
        } else if !self.overflow {
            self.buf[self.len] = b;
            self.len += 1;
        }
        Push::Pending
    }
}
