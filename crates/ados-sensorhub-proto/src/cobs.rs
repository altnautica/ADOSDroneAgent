//! Consistent Overhead Byte Stuffing (Cheshire & Baker), without the
//! trailing `0x00` delimiter (the frame layer appends it).

use crate::Error;

/// Worst-case encoded length of `n` input bytes.
pub const fn max_encoded_len(n: usize) -> usize {
    n + n / 254 + 1
}

/// Encode `src` into `dst`; returns the encoded length. The output holds no
/// `0x00`.
pub fn encode(src: &[u8], dst: &mut [u8]) -> Result<usize, Error> {
    if dst.len() < max_encoded_len(src.len()) {
        return Err(Error::BufferTooSmall);
    }
    let mut code_idx = 0;
    let mut out = 1;
    let mut code: u8 = 1;
    for &b in src {
        if b == 0 {
            dst[code_idx] = code;
            code_idx = out;
            out += 1;
            code = 1;
        } else {
            dst[out] = b;
            out += 1;
            code += 1;
            if code == 0xFF {
                dst[code_idx] = code;
                code_idx = out;
                out += 1;
                code = 1;
            }
        }
    }
    dst[code_idx] = code;
    Ok(out)
}

/// Decode one COBS block (no delimiter) from `src` into `dst`; returns the
/// decoded length.
pub fn decode(src: &[u8], dst: &mut [u8]) -> Result<usize, Error> {
    let mut i = 0;
    let mut out = 0;
    while i < src.len() {
        let code = src[i];
        if code == 0 {
            return Err(Error::Cobs);
        }
        i += 1;
        let run = usize::from(code) - 1;
        if i + run > src.len() {
            return Err(Error::Cobs);
        }
        if out + run > dst.len() {
            return Err(Error::BufferTooSmall);
        }
        for &b in &src[i..i + run] {
            if b == 0 {
                return Err(Error::Cobs);
            }
        }
        dst[out..out + run].copy_from_slice(&src[i..i + run]);
        out += run;
        i += run;
        if code != 0xFF && i < src.len() {
            if out >= dst.len() {
                return Err(Error::BufferTooSmall);
            }
            dst[out] = 0;
            out += 1;
        }
    }
    Ok(out)
}
