//! Raspberry Pi throttle-flag reader (`vcgencmd get_throttled`).
//!
//! On the Broadcom/Pi family the firmware exposes its own statement of whether
//! the board has under-volted, hit a thermal/soft temperature limit, or had its
//! frequency capped, as a bitfield from `vcgencmd get_throttled`. This is the
//! board's own word that it throttled, which no sysfs temperature alone proves.
//!
//! The reader is gated on the SoC family so a non-Pi board never spawns a
//! missing `vcgencmd`. The subprocess runs with a bounded timeout and is killed
//! on overrun so a hung firmware call never accumulates; a failed or absent
//! call records no flags for that tick (graceful skip).

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

use super::soc::SocFamily;

/// How long `vcgencmd` is allowed to run before it is killed. The call is cheap
/// in the common case; the bound guards against a hung firmware mailbox.
pub const VCGENCMD_TIMEOUT: Duration = Duration::from_secs(2);

/// Bit positions in the `get_throttled` bitfield. The low bits are the
/// currently-active states; the high bits (`+16`) latch that the state occurred
/// since boot. The collector surfaces the currently-active low bits.
const BIT_UNDER_VOLTAGE: u32 = 1 << 0;
const BIT_FREQ_CAPPED: u32 = 1 << 1;
const BIT_THROTTLED: u32 = 1 << 2;
const BIT_SOFT_TEMP_LIMIT: u32 = 1 << 3;

/// The decoded throttle state for one tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Throttle {
    /// The raw bitfield value.
    pub raw: u32,
    /// Under-voltage currently detected.
    pub under_voltage: bool,
    /// ARM frequency currently capped.
    pub freq_capped: bool,
    /// Currently throttled.
    pub throttled: bool,
    /// Soft temperature limit currently active.
    pub soft_temp_limit: bool,
}

/// Decode a `get_throttled` raw bitfield into the active low-bit flags.
pub fn decode_throttle(raw: u32) -> Throttle {
    Throttle {
        raw,
        under_voltage: raw & BIT_UNDER_VOLTAGE != 0,
        freq_capped: raw & BIT_FREQ_CAPPED != 0,
        throttled: raw & BIT_THROTTLED != 0,
        soft_temp_limit: raw & BIT_SOFT_TEMP_LIMIT != 0,
    }
}

/// Parse the `vcgencmd get_throttled` stdout line `throttled=0x50000` into the
/// raw bitfield. Returns `None` when the output does not carry a `throttled=`
/// hex value.
pub fn parse_get_throttled(stdout: &str) -> Option<u32> {
    let value = stdout.split('=').nth(1)?.trim();
    let hex = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    u32::from_str_radix(hex, 16).ok()
}

/// Read the Pi throttle flags. Returns `None` on a non-Pi board (gated, never
/// spawns), on a `vcgencmd` that is absent / errors / times out, or on output
/// that does not parse. A `Some` always carries a decoded bitfield.
pub async fn read_throttle(family: SocFamily) -> Option<Throttle> {
    if family != SocFamily::Broadcom {
        return None;
    }
    let raw = run_vcgencmd_get_throttled().await?;
    Some(decode_throttle(raw))
}

/// Spawn `vcgencmd get_throttled` with a bounded timeout and a kill on overrun,
/// returning the parsed raw bitfield. Any spawn / exit / timeout / parse failure
/// is a graceful `None`.
async fn run_vcgencmd_get_throttled() -> Option<u32> {
    let child = Command::new("vcgencmd")
        .arg("get_throttled")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;

    let wait = child.wait_with_output();
    match timeout(VCGENCMD_TIMEOUT, wait).await {
        Ok(Ok(output)) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            parse_get_throttled(&stdout)
        }
        // Non-zero exit, spawn-side IO error, or the timeout elapsed (the child
        // is killed on drop): record no flags for this tick.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_typical_throttled_line() {
        assert_eq!(parse_get_throttled("throttled=0x50000\n"), Some(0x50000));
        assert_eq!(parse_get_throttled("throttled=0x0\n"), Some(0));
        assert_eq!(parse_get_throttled("throttled=0X5"), Some(5));
    }

    #[test]
    fn rejects_output_without_a_value() {
        assert_eq!(parse_get_throttled("garbage"), None);
        assert_eq!(parse_get_throttled("throttled="), None);
        assert_eq!(parse_get_throttled("throttled=zz"), None);
    }

    #[test]
    fn decodes_the_active_low_bits() {
        // 0x1 = under-voltage now; 0x50000 = high latched bits only (no active
        // low bit) -> all current flags clear.
        let now = decode_throttle(0x1);
        assert!(now.under_voltage);
        assert!(!now.freq_capped);

        let capped = decode_throttle(0x2);
        assert!(capped.freq_capped);

        let throttled = decode_throttle(0x4);
        assert!(throttled.throttled);

        let soft = decode_throttle(0x8);
        assert!(soft.soft_temp_limit);

        let clean = decode_throttle(0x50000);
        assert_eq!(clean.raw, 0x50000);
        assert!(!clean.under_voltage);
        assert!(!clean.freq_capped);
        assert!(!clean.throttled);
        assert!(!clean.soft_temp_limit);
    }

    #[tokio::test]
    async fn non_pi_family_never_reads_throttle() {
        // SocFamily::Other is gated out before any subprocess spawn.
        assert_eq!(read_throttle(SocFamily::Other).await, None);
    }
}
