//! Host clock reads shared by every sensor-bus producer.

use nix::time::{clock_gettime, ClockId};

fn read(id: ClockId) -> i128 {
    match clock_gettime(id) {
        Ok(ts) => ts.tv_sec() as i128 * 1_000_000_000 + ts.tv_nsec() as i128,
        Err(_) => 0,
    }
}

/// Host `CLOCK_MONOTONIC`, nanoseconds: the bus timebase.
pub fn mono_ns() -> u64 {
    read(ClockId::CLOCK_MONOTONIC).max(0) as u64
}

/// Host `CLOCK_REALTIME`, nanoseconds since the Unix epoch.
pub fn realtime_ns() -> i64 {
    read(ClockId::CLOCK_REALTIME) as i64
}

#[cfg(test)]
mod tests {
    #[test]
    fn monotonic_advances() {
        let a = super::mono_ns();
        let b = super::mono_ns();
        assert!(a > 0 && b >= a);
        assert!(super::realtime_ns() > 1_600_000_000_000_000_000);
    }
}
