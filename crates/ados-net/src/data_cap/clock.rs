//! Calendar-month and wall-clock helpers.

use super::*;

/// Local-time `%Y-%m`, matching Python `time.strftime("%Y-%m")`.
pub(super) fn current_month() -> String {
    // Derive year-month from local time without a chrono dependency. We read
    // the local broken-down time via libc on unix; off-unix we fall back to a
    // UTC computation. Both render `YYYY-MM`.
    local_year_month().unwrap_or_else(utc_year_month)
}

#[cfg(unix)]
fn local_year_month() -> Option<String> {
    // SAFETY: localtime_r writes into a caller-owned tm; time() needs no args.
    unsafe {
        let t = libc_time();
        let mut tm: LibcTm = std::mem::zeroed();
        if localtime_r(&t, &mut tm).is_null() {
            return None;
        }
        Some(format!("{:04}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1))
    }
}

#[cfg(not(unix))]
fn local_year_month() -> Option<String> {
    None
}

// Minimal libc FFI for localtime_r so the crate does not pull a date library
// for a single `%Y-%m` format. tm layout matches the C `struct tm` prefix we
// read (year + month); the remaining fields are present so the struct size is
// correct for the FFI call.
#[cfg(unix)]
#[repr(C)]
struct LibcTm {
    tm_sec: i32,
    tm_min: i32,
    tm_hour: i32,
    tm_mday: i32,
    tm_mon: i32,
    tm_year: i32,
    tm_wday: i32,
    tm_yday: i32,
    tm_isdst: i32,
    tm_gmtoff: i64,
    tm_zone: *const i8,
}

#[cfg(unix)]
extern "C" {
    #[link_name = "time"]
    fn c_time(tloc: *mut i64) -> i64;
    fn localtime_r(timep: *const i64, result: *mut LibcTm) -> *mut LibcTm;
}

#[cfg(unix)]
unsafe fn libc_time() -> i64 {
    c_time(std::ptr::null_mut())
}

/// UTC fallback year-month from the unix epoch (no leap-second handling; only
/// used off-unix or if localtime_r fails).
fn utc_year_month() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let days = secs.div_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}")
}

pub(super) fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Round to 2 decimals the same way Python `round(x, 2)` does for the percent
/// field (banker's rounding is not observable at the precision we emit).
pub(super) fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}
