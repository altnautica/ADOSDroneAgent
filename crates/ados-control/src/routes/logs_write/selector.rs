//! Selector validation (`parse_since` / `validate_kinds`), with the typed error.

use super::*;

/// A validation failure that maps to a `400 {"error": {"code, message}}` body,
/// carrying the stable code the residual `LogPushTriggerError` carries.
#[derive(Debug)]
pub(super) struct TriggerError {
    pub(super) code: &'static str,
    pub(super) message: String,
}

impl TriggerError {
    pub(super) fn response(&self) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"code": self.code, "message": self.message}})),
        )
            .into_response()
    }
}

/// The validated window selector ready to be written as a trigger file. Mirrors
/// the Python `PushRequest`.
pub(super) struct PushRequest {
    pub(super) session: Option<i64>,
    pub(super) since_us: Option<i64>,
    pub(super) kinds: Vec<String>,
}

/// Resolve a `since` string to an epoch-microsecond lower bound, accepting the
/// same three forms the store's query API accepts (mirrors `parse_since`):
/// a relative `-Ns`/`-Nm`/`-Nh`/`-Nd`/`-Nms`, an absolute epoch-microsecond
/// integer, or a bare `YYYY-MM-DDTHH:MM:SS` (space/`T` separator) read as UTC.
/// `None` for an empty/absent value; `Err(bad_since)` on a malformed input.
pub(super) fn parse_since(value: Option<&str>, now_us: i64) -> Result<Option<i64>, TriggerError> {
    let s = match value {
        None => return Ok(None),
        Some(v) => v.trim(),
    };
    if s.is_empty() {
        return Ok(None);
    }

    if let Some(rest) = s.strip_prefix('-') {
        match parse_relative(rest) {
            Some(micros) => return Ok(Some(now_us - micros)),
            None => {
                return Err(TriggerError {
                    code: "bad_since",
                    message: format!("{s:?} is not a relative duration like -5m, -2h, -1d"),
                });
            }
        }
    }

    if let Ok(n) = s.parse::<i64>() {
        return Ok(Some(n));
    }

    if let Some(iso) = parse_iso_utc(s) {
        return Ok(Some(iso));
    }

    Err(TriggerError {
        code: "bad_since",
        message: format!(
            "{s:?} is not an epoch-microsecond integer, an ISO timestamp, \
or a relative duration like -5m"
        ),
    })
}

/// Parse a `90s` / `5m` / `2h` / `1d` / `500ms` magnitude+unit into microseconds.
/// `None` on a malformed input. Matches the Python `_parse_relative`.
pub(super) fn parse_relative(rest: &str) -> Option<i64> {
    let rest = rest.trim();
    // Split at the first ASCII alphabetic char (the unit), as the Python does:
    // `next((i for i,c in enumerate(rest) if c.isascii() and c.isalpha()), None)`.
    let split = rest
        .char_indices()
        .find(|(_, c)| c.is_ascii_alphabetic())
        .map(|(i, _)| i)?;
    let (num, unit) = rest.split_at(split);
    let magnitude: i64 = num.trim().parse().ok()?;
    if magnitude < 0 {
        return None;
    }
    let per_unit_us: i64 = match unit {
        "ms" => 1_000,
        "s" => 1_000_000,
        "m" => 60 * 1_000_000,
        "h" => 3_600 * 1_000_000,
        "d" => 86_400 * 1_000_000,
        _ => return None,
    };
    Some(magnitude * per_unit_us)
}

/// Parse a bare `YYYY-MM-DDTHH:MM:SS` (space or `T` separator), read as UTC, into
/// an epoch-microsecond integer. `None` when it is not that shape. Mirrors the
/// Python `_parse_iso_utc` (`datetime.fromisoformat` after normalizing the
/// separator + stripping a trailing `Z`, then read as UTC when naive).
pub(super) fn parse_iso_utc(s: &str) -> Option<i64> {
    let normalized = s.replace(' ', "T");
    let normalized = normalized.strip_suffix('Z').unwrap_or(&normalized);
    let secs = civil_to_unix_secs(normalized)?;
    Some(secs * 1_000_000)
}

/// Parse a naive `YYYY-MM-DDTHH:MM:SS` (no offset; an offset makes this `None`
/// since the residual `_parse_iso_utc` only ever feeds a naive timestamp through —
/// `fromisoformat` of an aware string keeps its own tz, but the bare form the
/// store query uses is naive) into a Unix-epoch second count read as UTC. The
/// civil-to-days conversion is the inverse of the sibling read module's
/// `iso8601_from_unix_secs`.
pub(super) fn civil_to_unix_secs(s: &str) -> Option<i64> {
    let (date, time) = s.split_once('T')?;
    let mut dparts = date.split('-');
    let year: i64 = dparts.next()?.parse().ok()?;
    let month: i64 = dparts.next()?.parse().ok()?;
    let day: i64 = dparts.next()?.parse().ok()?;
    if dparts.next().is_some() {
        return None;
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let mut tparts = time.split(':');
    let hour: i64 = tparts.next()?.parse().ok()?;
    let minute: i64 = tparts.next()?.parse().ok()?;
    let sec_str = tparts.next()?;
    if tparts.next().is_some() {
        return None;
    }
    // A fractional second is accepted (truncated to whole seconds; the store query
    // form is whole-second, and `fromisoformat` would carry the fraction, but the
    // epoch-second floor is what the lower bound needs).
    let second: i64 = match sec_str.split_once('.') {
        Some((whole, _frac)) => whole.parse().ok()?,
        None => sec_str.parse().ok()?,
    };
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=60).contains(&second) {
        return None;
    }

    // Howard Hinnant's days_from_civil: (y, m, d) → days since the Unix epoch.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    let days = era * 146_097 + doe - 719_468;

    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Normalize and validate the requested window kinds. An empty/absent list selects
/// all four; an unknown kind is `Err(bad_kind)`; order is preserved and duplicates
/// dropped. Matches the Python `validate_kinds`.
pub(super) fn validate_kinds(kinds: Option<Vec<String>>) -> Result<Vec<String>, TriggerError> {
    let kinds = match kinds {
        Some(k) if !k.is_empty() => k,
        _ => return Ok(VALID_KINDS.iter().map(|s| s.to_string()).collect()),
    };
    let mut seen: Vec<String> = Vec::new();
    for raw in &kinds {
        let k = raw.trim().to_ascii_lowercase();
        if !VALID_KINDS.contains(&k.as_str()) {
            return Err(TriggerError {
                code: "bad_kind",
                message: format!(
                    "unknown kind {raw:?}; choose from {}",
                    VALID_KINDS.join(", ")
                ),
            });
        }
        if !seen.contains(&k) {
            seen.push(k);
        }
    }
    Ok(seen)
}

/// Validate the operator's selector into a `PushRequest`, mirroring the Python
/// `build_request` (`parse_since` then `validate_kinds`).
pub(super) fn build_request(
    session: Option<i64>,
    since: Option<&str>,
    kinds: Option<Vec<String>>,
    now_us: i64,
) -> Result<PushRequest, TriggerError> {
    let since_us = parse_since(since, now_us)?;
    let kinds = validate_kinds(kinds)?;
    Ok(PushRequest {
        session,
        since_us,
        kinds,
    })
}
