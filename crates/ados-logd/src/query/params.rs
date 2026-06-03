//! Query-string parsing for the read endpoints.
//!
//! Every endpoint shares a small set of filter knobs: a time window (absolute
//! microsecond epoch or a relative `-5m` form), a table selector, source /
//! metric / event-kind lists, a level floor, a text match, a session
//! restriction, a page size, and an opaque cursor. This module turns the raw
//! `key=value` pairs into the typed [`QueryFilters`] the SQL builder consumes,
//! and computes the filter fingerprint a cursor is bound to.

use std::collections::HashMap;

use ados_protocol::logd::Level;

use super::pagination::FilterFingerprint;

/// Which table a `query`/`export`/`tail` request reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    /// Log records (the default).
    Logs,
    /// Discrete events.
    Events,
    /// Telemetry samples.
    Metrics,
    /// Hardware snapshots.
    Hw,
}

impl Table {
    /// Parse the `kind` selector, defaulting to `logs`.
    pub fn parse(s: Option<&str>) -> Result<Table, ParamError> {
        match s.unwrap_or("logs") {
            "logs" => Ok(Table::Logs),
            "events" => Ok(Table::Events),
            "metrics" => Ok(Table::Metrics),
            "hw" => Ok(Table::Hw),
            other => Err(ParamError::new(
                "bad_kind",
                format!("unknown kind '{other}', expected logs|events|metrics|hw"),
            )),
        }
    }

    /// The SQL table name.
    pub fn name(self) -> &'static str {
        match self {
            Table::Logs => "logs",
            Table::Events => "events",
            Table::Metrics => "metrics",
            Table::Hw => "hw",
        }
    }

    /// A stable label for the fingerprint.
    pub fn label(self) -> &'static str {
        self.name()
    }
}

/// The default page size when none is given.
pub const DEFAULT_LIMIT: u32 = 200;

/// The hard cap on a single page, so a runaway client cannot ask for the world.
pub const MAX_LIMIT: u32 = 2000;

/// A parse/validation error carrying a stable bland code and a message; maps to
/// the `{ error: { code, message } }` envelope with HTTP 400.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamError {
    /// Stable machine code.
    pub code: String,
    /// Human-readable detail.
    pub message: String,
}

impl ParamError {
    /// Build a parameter error.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// The typed filter set shared by `query`, `tail`, and `export`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryFilters {
    /// Which table to read.
    pub table: Table,
    /// Inclusive lower bound, microsecond epoch.
    pub from_us: Option<i64>,
    /// Exclusive upper bound, microsecond epoch.
    pub to_us: Option<i64>,
    /// Restrict to these emitting sources (logs/events/hw — n/a for metrics).
    pub sources: Vec<String>,
    /// Restrict to these dotted metric keys (metrics only).
    pub metrics: Vec<String>,
    /// Restrict to these event kinds (events only).
    pub event_kinds: Vec<String>,
    /// Minimum severity level, inclusive (logs/events only).
    pub min_level: Option<Level>,
    /// Free-text substring against `msg` / `target`.
    pub text: Option<String>,
    /// Restrict to one session id.
    pub session: Option<i64>,
    /// Page size, clamped to [`MAX_LIMIT`].
    pub limit: u32,
    /// The opaque cursor token, if a follow-on page was requested.
    pub cursor: Option<String>,
    /// Restrict to rows not yet marked synced (`?unsynced=1`). Used by the
    /// explicit window export so the exported bytes are exactly the rows the
    /// matching mark-synced flip will flip, keeping the export deterministic.
    pub unsynced_only: bool,
}

impl QueryFilters {
    /// Parse the filter set from a decoded query map. `now_us` anchors any
    /// relative time bound. Unknown keys are ignored (forward compatibility);
    /// malformed values are rejected.
    pub fn parse(params: &QueryParams, now_us: i64) -> Result<QueryFilters, ParamError> {
        let table = Table::parse(params.get("kind"))?;
        let from_us = parse_time(params.get("from"), now_us, "from")?;
        let to_us = parse_time(params.get("to"), now_us, "to")?;
        // `since` is a friendly alias for a relative lower bound; it sets `from`
        // when `from` is absent.
        let from_us = match (from_us, params.get("since")) {
            (None, Some(since)) => parse_time(Some(since), now_us, "since")?,
            (existing, _) => existing,
        };
        if let (Some(lo), Some(hi)) = (from_us, to_us) {
            if lo > hi {
                return Err(ParamError::new("bad_range", "from is after to"));
            }
        }
        let min_level = match params.get("level") {
            Some(l) => Some(parse_level(l)?),
            None => None,
        };
        let limit = match params.get("limit") {
            Some(l) => l
                .parse::<u32>()
                .map_err(|_| ParamError::new("bad_limit", format!("limit '{l}' is not a number")))?
                .clamp(1, MAX_LIMIT),
            None => DEFAULT_LIMIT,
        };
        let session = match params.get("session") {
            Some(s) => Some(s.parse::<i64>().map_err(|_| {
                ParamError::new("bad_session", format!("session '{s}' is not a number"))
            })?),
            None => None,
        };
        Ok(QueryFilters {
            table,
            from_us,
            to_us,
            sources: params.get_all("source"),
            metrics: params.get_all("metric"),
            event_kinds: params.get_all("event_kind"),
            min_level,
            text: params
                .get("text")
                .map(str::to_string)
                .filter(|s| !s.is_empty()),
            session,
            limit,
            cursor: params.get("cursor").map(str::to_string),
            unsynced_only: matches!(params.get("unsynced"), Some("1") | Some("true")),
        })
    }

    /// The fingerprint a cursor is bound to: every value that shapes the result
    /// set, in a fixed order so the fingerprint is stable across requests.
    pub fn fingerprint(&self) -> u64 {
        let mut fp = FilterFingerprint::new();
        fp.add_str(self.table.label())
            .add_opt_i64(self.from_us)
            .add_opt_i64(self.to_us)
            .add_opt_i64(self.min_level.map(|l| i64::from(l.as_u8())))
            .add_opt_str(self.text.as_deref())
            .add_opt_i64(self.session);
        // List filters are order-insensitive in SQL (`IN (...)`), so sort the
        // copies before folding them in to keep the fingerprint stable
        // regardless of the order the client listed them.
        fp.add_str_list(&sorted(&self.sources))
            .add_str_list(&sorted(&self.metrics))
            .add_str_list(&sorted(&self.event_kinds));
        // The unsynced restriction changes the result set, so an unsynced-only
        // cursor must not be replayable against a full export and vice versa.
        fp.add_i64(self.unsynced_only as i64);
        fp.finish()
    }
}

/// A sorted copy of a string list (for the order-insensitive fingerprint).
fn sorted(items: &[String]) -> Vec<String> {
    let mut v = items.to_vec();
    v.sort();
    v
}

/// Parse a level filter from an ordinal (`0`..`4`) or a name (`trace`..`error`).
pub fn parse_level(s: &str) -> Result<Level, ParamError> {
    if let Ok(n) = s.parse::<u8>() {
        if n <= 4 {
            return Ok(Level::from_u8(n));
        }
    }
    match s.to_ascii_lowercase().as_str() {
        "trace" => Ok(Level::Trace),
        "debug" => Ok(Level::Debug),
        "info" => Ok(Level::Info),
        "warn" | "warning" => Ok(Level::Warn),
        "error" => Ok(Level::Error),
        _ => Err(ParamError::new(
            "bad_level",
            format!("unknown level '{s}', expected trace|debug|info|warn|error or 0..4"),
        )),
    }
}

/// Parse a time bound. Accepts an absolute microsecond epoch integer, an ISO-ish
/// `YYYY-MM-DDTHH:MM:SS` (interpreted as UTC), or a relative `-<n><unit>` form
/// (`-5m`, `-2h`, `-90s`, `-1d`) resolved against `now_us`.
pub fn parse_time(s: Option<&str>, now_us: i64, field: &str) -> Result<Option<i64>, ParamError> {
    let Some(s) = s else { return Ok(None) };
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    // Relative form: a leading '-' followed by a magnitude and a unit suffix.
    if let Some(rest) = s.strip_prefix('-') {
        let micros = parse_relative(rest).ok_or_else(|| {
            ParamError::new(
                "bad_time",
                format!("{field} '{s}' is not a relative duration like -5m, -2h, -1d"),
            )
        })?;
        return Ok(Some(now_us.saturating_sub(micros)));
    }
    // Absolute microsecond epoch integer.
    if let Ok(n) = s.parse::<i64>() {
        return Ok(Some(n));
    }
    // ISO-ish absolute timestamp.
    if let Some(us) = parse_iso_utc(s) {
        return Ok(Some(us));
    }
    Err(ParamError::new(
        "bad_time",
        format!("{field} '{s}' is not an epoch-microsecond integer, an ISO timestamp, or a relative duration"),
    ))
}

/// Parse a relative magnitude+unit (`90s`, `5m`, `2h`, `1d`, `500ms`) into
/// microseconds. Returns `None` on a malformed input.
fn parse_relative(rest: &str) -> Option<i64> {
    let rest = rest.trim();
    // Split the trailing alphabetic unit off the leading numeric magnitude.
    let split = rest.find(|c: char| c.is_ascii_alphabetic())?;
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
    magnitude.checked_mul(per_unit_us)
}

/// Parse a bare `YYYY-MM-DDTHH:MM:SS` (or with a space separator), interpreted
/// as UTC, into microsecond epoch. Deliberately small: a full RFC-3339 parser
/// is not pulled in for this convenience path.
fn parse_iso_utc(s: &str) -> Option<i64> {
    let s = s.replace(' ', "T");
    let (date, time) = s.split_once('T')?;
    let mut dparts = date.split('-');
    let year: i64 = dparts.next()?.parse().ok()?;
    let month: i64 = dparts.next()?.parse().ok()?;
    let day: i64 = dparts.next()?.parse().ok()?;
    // Trim a trailing 'Z' and any fractional seconds.
    let time = time.trim_end_matches('Z');
    let time = time.split('.').next().unwrap_or(time);
    let mut tparts = time.split(':');
    let hour: i64 = tparts.next()?.parse().ok()?;
    let minute: i64 = tparts.next().unwrap_or("0").parse().ok()?;
    let second: i64 = tparts.next().unwrap_or("0").parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second;
    Some(secs * 1_000_000)
}

/// Days since the Unix epoch for a civil (proleptic Gregorian) date. The
/// standard branch-free algorithm; correct for all dates the store ever holds.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// A decoded query string: the multimap of keys to values. Repeated keys (the
/// `source[]` / `metric[]` lists) keep every value in order.
#[derive(Debug, Default, Clone)]
pub struct QueryParams {
    multi: HashMap<String, Vec<String>>,
}

impl QueryParams {
    /// Decode a raw query string (`a=1&b=2&source=x&source=y`). Percent-decoding
    /// is applied to both keys and values. An empty/absent string yields an
    /// empty map.
    pub fn parse(query: &str) -> QueryParams {
        let mut multi: HashMap<String, Vec<String>> = HashMap::new();
        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (k, v) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                None => (pair, ""),
            };
            let key = percent_decode(k);
            let val = percent_decode(v);
            multi.entry(key).or_default().push(val);
        }
        QueryParams { multi }
    }

    /// The first value for a key, if present.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.multi
            .get(key)
            .and_then(|v| v.first())
            .map(String::as_str)
    }

    /// Every value for a key (for the repeated list filters), dropping empties.
    pub fn get_all(&self, key: &str) -> Vec<String> {
        self.multi
            .get(key)
            .map(|vs| vs.iter().filter(|s| !s.is_empty()).cloned().collect())
            .unwrap_or_default()
    }
}

/// Percent-decode a query-string token, turning `+` into a space and `%XX` into
/// its byte. Invalid escapes are passed through verbatim.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = hex_val(bytes[i + 1]);
                let lo = hex_val(bytes[i + 2]);
                match (hi, lo) {
                    (Some(hi), Some(lo)) => {
                        out.push(hi << 4 | lo);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_params_split_repeated_keys_and_decode() {
        let p = QueryParams::parse(
            "kind=logs&source=api&source=ados-video&text=hello%20world&limit=50",
        );
        assert_eq!(p.get("kind"), Some("logs"));
        assert_eq!(p.get("text"), Some("hello world"));
        assert_eq!(p.get("limit"), Some("50"));
        assert_eq!(
            p.get_all("source"),
            vec!["api".to_string(), "ados-video".to_string()]
        );
    }

    #[test]
    fn empty_query_is_an_empty_map() {
        let p = QueryParams::parse("");
        assert_eq!(p.get("kind"), None);
        assert!(p.get_all("source").is_empty());
    }

    #[test]
    fn relative_time_bounds_resolve_against_now() {
        let now = 10_000_000_000; // 10_000 seconds in micros
        assert_eq!(
            parse_time(Some("-5s"), now, "from").unwrap(),
            Some(now - 5_000_000)
        );
        assert_eq!(
            parse_time(Some("-2m"), now, "from").unwrap(),
            Some(now - 120_000_000)
        );
        assert_eq!(
            parse_time(Some("-1h"), now, "from").unwrap(),
            Some(now - 3_600_000_000)
        );
        assert_eq!(
            parse_time(Some("-1d"), now, "from").unwrap(),
            Some(now - 86_400_000_000)
        );
        assert_eq!(
            parse_time(Some("-500ms"), now, "from").unwrap(),
            Some(now - 500_000)
        );
    }

    #[test]
    fn absolute_epoch_and_iso_time_bounds() {
        assert_eq!(
            parse_time(Some("1700000000000000"), 0, "from").unwrap(),
            Some(1_700_000_000_000_000)
        );
        // 1970-01-01T00:00:01Z == 1_000_000 micros.
        assert_eq!(
            parse_time(Some("1970-01-01T00:00:01Z"), 0, "from").unwrap(),
            Some(1_000_000)
        );
        // A space separator and no zone are accepted.
        assert_eq!(
            parse_time(Some("1970-01-01 00:01:00"), 0, "from").unwrap(),
            Some(60_000_000)
        );
    }

    #[test]
    fn malformed_time_is_rejected() {
        assert!(parse_time(Some("-5x"), 0, "from").is_err());
        assert!(parse_time(Some("not a time"), 0, "from").is_err());
        assert_eq!(parse_time(None, 0, "from").unwrap(), None);
        assert_eq!(parse_time(Some(""), 0, "from").unwrap(), None);
    }

    #[test]
    fn level_parses_names_and_ordinals() {
        assert_eq!(parse_level("warn").unwrap(), Level::Warn);
        assert_eq!(parse_level("ERROR").unwrap(), Level::Error);
        assert_eq!(parse_level("3").unwrap(), Level::Warn);
        assert_eq!(parse_level("0").unwrap(), Level::Trace);
        assert!(parse_level("9").is_err());
        assert!(parse_level("loud").is_err());
    }

    #[test]
    fn since_aliases_a_relative_from_when_from_is_absent() {
        let now = 5_000_000_000;
        let p = QueryParams::parse("since=-1m");
        let f = QueryFilters::parse(&p, now).unwrap();
        assert_eq!(f.from_us, Some(now - 60_000_000));
    }

    #[test]
    fn limit_is_clamped_and_defaults() {
        let now = 0;
        assert_eq!(
            QueryFilters::parse(&QueryParams::parse(""), now)
                .unwrap()
                .limit,
            DEFAULT_LIMIT
        );
        assert_eq!(
            QueryFilters::parse(&QueryParams::parse("limit=999999"), now)
                .unwrap()
                .limit,
            MAX_LIMIT
        );
        assert_eq!(
            QueryFilters::parse(&QueryParams::parse("limit=0"), now)
                .unwrap()
                .limit,
            1
        );
    }

    #[test]
    fn fingerprint_ignores_source_list_order() {
        let a = QueryFilters::parse(&QueryParams::parse("source=a&source=b"), 0).unwrap();
        let b = QueryFilters::parse(&QueryParams::parse("source=b&source=a"), 0).unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn fingerprint_changes_with_table_and_level() {
        let logs = QueryFilters::parse(&QueryParams::parse("kind=logs&level=warn"), 0).unwrap();
        let events = QueryFilters::parse(&QueryParams::parse("kind=events&level=warn"), 0).unwrap();
        let logs_info =
            QueryFilters::parse(&QueryParams::parse("kind=logs&level=info"), 0).unwrap();
        assert_ne!(logs.fingerprint(), events.fingerprint());
        assert_ne!(logs.fingerprint(), logs_info.fingerprint());
    }

    #[test]
    fn reversed_range_is_rejected() {
        let p = QueryParams::parse("from=200&to=100");
        assert!(QueryFilters::parse(&p, 0).is_err());
    }

    #[test]
    fn unsynced_flag_parses_and_changes_the_fingerprint() {
        let plain = QueryFilters::parse(&QueryParams::parse("kind=logs"), 0).unwrap();
        assert!(!plain.unsynced_only);
        let one = QueryFilters::parse(&QueryParams::parse("kind=logs&unsynced=1"), 0).unwrap();
        assert!(one.unsynced_only);
        let truthy =
            QueryFilters::parse(&QueryParams::parse("kind=logs&unsynced=true"), 0).unwrap();
        assert!(truthy.unsynced_only);
        // A non-true value leaves the flag off.
        let off = QueryFilters::parse(&QueryParams::parse("kind=logs&unsynced=0"), 0).unwrap();
        assert!(!off.unsynced_only);
        // The flag folds into the fingerprint so a cursor cannot cross between an
        // unsynced-only export and a full one.
        assert_ne!(plain.fingerprint(), one.fingerprint());
    }
}
