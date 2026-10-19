//! The host clock frame timestamps are taken on.

/// Host CLOCK_MONOTONIC in nanoseconds: the clock `ts_ns` and the tap header's
/// `ts_mono_ns` carry, shared by every process on the host. `0` if unreadable.
pub fn mono_ns() -> u64 {
    let mut ts = nix::libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec for the duration of the call.
    if unsafe { nix::libc::clock_gettime(nix::libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return 0;
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}
