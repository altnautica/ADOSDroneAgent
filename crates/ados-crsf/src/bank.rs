//! The live channel bank: the 16 RC channel values the fixed-cadence
//! transmitter reads each tick, plus where they came from.
//!
//! Injection validates against the usable endpoint range (172..=1811) at this
//! boundary, so an out-of-range command is rejected before it ever reaches
//! the wire; the codec below stays faithful to any 11-bit value.

use crate::channels::{CHANNEL_COUNT, CHANNEL_MAX, CHANNEL_MID, CHANNEL_MIN};

/// Where the current channel values came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelSource {
    /// Programmatic injection over the command socket.
    Api,
}

impl ChannelSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelSource::Api => "api",
        }
    }
}

/// Why a channel injection was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BankError {
    /// Channel index outside 0..16.
    BadIndex(usize),
    /// Value outside the usable 172..=1811 endpoint range.
    BadValue(u16),
}

/// The transmitted channel values plus their provenance. Held behind a mutex
/// and shared between the command socket (writer) and the TX task (reader).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelBank {
    values: [u16; CHANNEL_COUNT],
    source: Option<ChannelSource>,
}

impl Default for ChannelBank {
    /// Neutral defaults transmitted until a source injects real values:
    /// roll/pitch/yaw (channels 1/2/4 in AETR order) centered, throttle
    /// (channel 3) full low, every auxiliary channel full low — the
    /// stick-neutral, everything-off posture.
    fn default() -> Self {
        let mut values = [CHANNEL_MIN; CHANNEL_COUNT];
        values[0] = CHANNEL_MID; // roll
        values[1] = CHANNEL_MID; // pitch
        values[3] = CHANNEL_MID; // yaw
        Self {
            values,
            source: None,
        }
    }
}

impl ChannelBank {
    /// The current values, as transmitted each tick.
    pub fn values(&self) -> [u16; CHANNEL_COUNT] {
        self.values
    }

    /// Where the current values came from; `None` until first injection.
    pub fn source(&self) -> Option<ChannelSource> {
        self.source
    }

    fn check(value: u16) -> Result<(), BankError> {
        if (CHANNEL_MIN..=CHANNEL_MAX).contains(&value) {
            Ok(())
        } else {
            Err(BankError::BadValue(value))
        }
    }

    /// Replace all 16 channels. Rejects the whole set if any value is out of
    /// range — a partial apply would transmit a mixed frame nobody asked for.
    pub fn set_all(
        &mut self,
        values: [u16; CHANNEL_COUNT],
        source: ChannelSource,
    ) -> Result<(), BankError> {
        for &v in &values {
            Self::check(v)?;
        }
        self.values = values;
        self.source = Some(source);
        Ok(())
    }

    /// Set one channel by zero-based index.
    pub fn set_one(
        &mut self,
        index: usize,
        value: u16,
        source: ChannelSource,
    ) -> Result<(), BankError> {
        if index >= CHANNEL_COUNT {
            return Err(BankError::BadIndex(index));
        }
        Self::check(value)?;
        self.values[index] = value;
        self.source = Some(source);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_bank_is_stick_neutral_throttle_low() {
        let bank = ChannelBank::default();
        let v = bank.values();
        assert_eq!(v[0], CHANNEL_MID);
        assert_eq!(v[1], CHANNEL_MID);
        assert_eq!(v[2], CHANNEL_MIN, "throttle low");
        assert_eq!(v[3], CHANNEL_MID);
        assert!(v[4..].iter().all(|&x| x == CHANNEL_MIN));
        assert_eq!(bank.source(), None);
    }

    #[test]
    fn set_all_applies_and_records_the_source() {
        let mut bank = ChannelBank::default();
        let values = [CHANNEL_MID; CHANNEL_COUNT];
        bank.set_all(values, ChannelSource::Api).unwrap();
        assert_eq!(bank.values(), values);
        assert_eq!(bank.source(), Some(ChannelSource::Api));
    }

    #[test]
    fn set_all_rejects_any_out_of_range_value_atomically() {
        let mut bank = ChannelBank::default();
        let before = bank.values();
        let mut values = [CHANNEL_MID; CHANNEL_COUNT];
        values[9] = CHANNEL_MAX + 1;
        assert_eq!(
            bank.set_all(values, ChannelSource::Api),
            Err(BankError::BadValue(CHANNEL_MAX + 1))
        );
        assert_eq!(bank.values(), before, "no partial apply");
        assert_eq!(bank.source(), None);
    }

    #[test]
    fn set_one_bounds_index_and_value() {
        let mut bank = ChannelBank::default();
        bank.set_one(4, 1500, ChannelSource::Api).unwrap();
        assert_eq!(bank.values()[4], 1500);
        assert_eq!(
            bank.set_one(16, CHANNEL_MID, ChannelSource::Api),
            Err(BankError::BadIndex(16))
        );
        assert_eq!(
            bank.set_one(0, CHANNEL_MIN - 1, ChannelSource::Api),
            Err(BankError::BadValue(CHANNEL_MIN - 1))
        );
    }
}
