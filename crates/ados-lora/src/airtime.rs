//! LoRa time on air, from the Semtech SX1261/2 datasheet (section 6.1.4):
//!
//! ```text
//! T_sym = 2^SF / BW
//! T_pre = (N_pre + 4.25) * T_sym
//! N_pay = 8 + max(ceil((8 PL - 4 SF + 28 + 16 CRC - 20 IH) / (4 (SF - 2 DE))) * (CR + 4), 0)
//! T_oa  = T_pre + N_pay * T_sym
//! ```
//!
//! `DE` (low data-rate optimisation) is on when `T_sym >= 16 ms`, which is
//! SF11 and SF12 at 125 kHz. `CR` is 1..=4 for coding rates 4/5..4/8.

use serde::{Deserialize, Serialize};

/// The modem parameters a frame's time on air depends on.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Modem {
    /// Spreading factor, 7..=12.
    pub sf: u8,
    /// Bandwidth in kHz (125, 250, 500, ...).
    pub bw_khz: u32,
    /// Coding rate index 1..=4 (4/5..4/8).
    pub cr: u8,
    /// Preamble length in symbols.
    pub preamble: u16,
    /// Payload CRC on.
    pub crc: bool,
    /// Implicit header mode.
    pub implicit_header: bool,
}

impl Modem {
    /// SF9 / 125 kHz / 4-5, 8-symbol preamble, CRC on, explicit header: the
    /// assumption the E22 2.4k air rate is charged with, and the loopback default.
    pub const SF9_BW125: Modem = Modem {
        sf: 9,
        bw_khz: 125,
        cr: 1,
        preamble: 8,
        crc: true,
        implicit_header: false,
    };

    /// Symbol time in microseconds (`2^SF / BW`).
    pub fn symbol_us(&self) -> f64 {
        (1u64 << self.sf) as f64 * 1000.0 / self.bw_khz as f64
    }

    /// The SX126x demodulation SNR floor for this spreading factor, in dB
    /// (datasheet table: SF7 -7.5 dB stepping 2.5 dB down to SF12 -20 dB).
    pub fn snr_floor_db(&self) -> f64 {
        -7.5 - 2.5 * (f64::from(self.sf.clamp(7, 12)) - 7.0)
    }
}

/// Time on air of a `payload_len`-byte frame, in microseconds, rounded up.
pub fn time_on_air_us(m: &Modem, payload_len: usize) -> u64 {
    let t_sym = m.symbol_us();
    let de = if t_sym >= 16_000.0 { 1.0 } else { 0.0 };
    let sf = f64::from(m.sf);
    let crc = if m.crc { 1.0 } else { 0.0 };
    let ih = if m.implicit_header { 1.0 } else { 0.0 };
    let num = 8.0 * payload_len as f64 - 4.0 * sf + 28.0 + 16.0 * crc - 20.0 * ih;
    let den = 4.0 * (sf - 2.0 * de);
    let blocks = (num / den).ceil() * (f64::from(m.cr) + 4.0);
    let n_pay = 8.0 + blocks.max(0.0);
    let t_pre = (f64::from(m.preamble) + 4.25) * t_sym;
    (t_pre + n_pay * t_sym).ceil() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modem(sf: u8, bw_khz: u32, cr: u8, preamble: u16) -> Modem {
        Modem {
            sf,
            bw_khz,
            cr,
            preamble,
            crc: true,
            implicit_header: false,
        }
    }

    // Reference values computed by hand from the datasheet formula (each line
    // shows the symbol count), cross-checked against the worked example in the
    // LoRa lane specification (SF9/125/4-5, 22 bytes -> 205.8 ms).
    #[test]
    fn sf9_bw125_22_bytes_is_the_spec_worked_example() {
        // T_sym 4.096 ms; ceil((176-36+28+16)/36)=6 -> 6*5=30 -> N_pay 38;
        // (12.25 + 38) * 4.096 = 205.824 ms.
        assert_eq!(time_on_air_us(&Modem::SF9_BW125, 22), 205_824);
    }

    #[test]
    fn sf7_bw125_10_bytes() {
        // T_sym 1.024 ms; ceil((80-28+28+16)/28)=4 -> 20 -> N_pay 28;
        // (12.25 + 28) * 1.024 = 41.216 ms.
        assert_eq!(time_on_air_us(&modem(7, 125, 1, 8), 10), 41_216);
    }

    #[test]
    fn sf12_bw125_low_data_rate_optimisation_on() {
        // T_sym 32.768 ms -> DE on; ceil((408-48+28+16)/(4*10))=ceil(10.1)=11
        // -> 11*5=55 -> N_pay 63; (12.25 + 63) * 32.768 = 2465.792 ms.
        assert_eq!(time_on_air_us(&modem(12, 125, 1, 8), 51), 2_465_792);
    }

    #[test]
    fn sf10_bw250_cr48_16_symbol_preamble() {
        // T_sym 4.096 ms; ceil((256-40+28+16)/40)=ceil(6.5)=7 -> 7*8=56 ->
        // N_pay 64; (20.25 + 64) * 4.096 = 345.088 ms.
        assert_eq!(time_on_air_us(&modem(10, 250, 4, 16), 32), 345_088);
    }

    #[test]
    fn snr_floor_steps_by_spreading_factor() {
        assert_eq!(modem(7, 125, 1, 8).snr_floor_db(), -7.5);
        assert_eq!(modem(9, 125, 1, 8).snr_floor_db(), -12.5);
        assert_eq!(modem(12, 125, 1, 8).snr_floor_db(), -20.0);
    }
}
