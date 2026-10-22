//! CRC-16/CCITT-FALSE: poly 0x1021, init 0xFFFF, no reflection, xorout 0.

/// Initial register value.
pub const CRC_INIT: u16 = 0xFFFF;

/// Feed `data` into a running CRC register.
pub fn crc16_update(mut crc: u16, data: &[u8]) -> u16 {
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// CRC-16/CCITT-FALSE of `data`.
pub fn crc16(data: &[u8]) -> u16 {
    crc16_update(CRC_INIT, data)
}
