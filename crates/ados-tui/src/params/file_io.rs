//! `.param` files: `NAME,VALUE` lines (also `NAME VALUE`), `#` comments.

use super::metadata::plain;
use super::store::{StageOutcome, Store};
use super::same;

/// What an import did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportCounts {
    pub staged: usize,
    pub unchanged: usize,
    /// Names the FC does not have, plus lines that do not parse.
    pub unknown: usize,
    /// Parameters the agent refuses to write.
    pub read_only: usize,
}

impl ImportCounts {
    pub fn summary(&self) -> String {
        let mut text = format!(
            "Staged {}, unchanged {}, unknown {}",
            self.staged, self.unchanged, self.unknown
        );
        if self.read_only > 0 {
            text.push_str(&format!(", read-only {}", self.read_only));
        }
        text
    }
}

/// The live values as sorted `NAME,VALUE` lines.
pub fn export_text(store: &Store) -> String {
    let mut out = String::new();
    for (name, value) in store.live() {
        out.push_str(name);
        out.push(',');
        out.push_str(&plain(*value));
        out.push('\n');
    }
    out
}

/// One `NAME,VALUE` / `NAME VALUE` line; `None` for a blank or comment line,
/// `Some(Err)` for a line that does not parse.
pub fn parse_line(line: &str) -> Option<Result<(String, f64), ()>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut parts = line
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|p| !p.is_empty());
    let parsed = match (parts.next(), parts.next()) {
        (Some(name), Some(value)) => value
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .map(|v| (name.to_string(), v))
            .ok_or(()),
        _ => Err(()),
    };
    Some(parsed)
}

/// Stage every value in `text` that differs from the live one.
pub fn import(store: &mut Store, text: &str) -> ImportCounts {
    let mut counts = ImportCounts::default();
    for parsed in text.lines().filter_map(parse_line) {
        let Ok((name, value)) = parsed else {
            counts.unknown += 1;
            continue;
        };
        let Some(live) = store.value(&name) else {
            counts.unknown += 1;
            continue;
        };
        if same(live, value) {
            store.unstage(&name);
            counts.unchanged += 1;
            continue;
        }
        match store.stage(&name, value) {
            StageOutcome::Staged => counts.staged += 1,
            StageOutcome::Unchanged => counts.unchanged += 1,
            StageOutcome::ReadOnly => counts.read_only += 1,
        }
    }
    counts
}

/// `<device>-<YYYYMMDD-HHMMSS>.param` (UTC) with the device name reduced to
/// filename-safe characters.
pub fn export_name(device: &str, unix_secs: u64) -> String {
    let safe: String = device
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect();
    let device = if safe.trim_matches('_').is_empty() { "ados".to_string() } else { safe };
    format!("{device}-{}.param", stamp(unix_secs))
}

/// `YYYYMMDD-HHMMSS` for a Unix time (UTC).
pub fn stamp(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let secs = unix_secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Gregorian date of a day count since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(values: &[(&str, f64)]) -> Store {
        let mut s = Store::default();
        s.set_live(values.iter().map(|(n, v)| (n.to_string(), *v)).collect());
        s
    }

    #[test]
    fn import_counts_staged_unchanged_and_unknown() {
        let mut s = store(&[("A_ONE", 1.0), ("B_TWO", 2.0), ("SYSID_THISMAV", 1.0)]);
        let text = "# comment\nA_ONE,5\nB_TWO 2\nZ_NONE,3\nnot a number\n\nSYSID_THISMAV,2\n";
        let c = import(&mut s, text);
        assert_eq!(
            c,
            ImportCounts {
                staged: 1,
                unchanged: 1,
                unknown: 2,
                read_only: 1
            }
        );
        assert_eq!(s.staged_value("A_ONE"), Some(5.0));
        assert_eq!(c.summary(), "Staged 1, unchanged 1, unknown 2, read-only 1");
    }

    #[test]
    fn export_then_import_round_trips() {
        let source = store(&[("ATC_RAT_RLL_P", 0.135_000_005_364_418_03), ("WPNAV_SPEED", 500.0), ("BATT_LOW_VOLT", 10.5)]);
        let text = export_text(&source);
        assert_eq!(text, "ATC_RAT_RLL_P,0.135\nBATT_LOW_VOLT,10.5\nWPNAV_SPEED,500\n");

        let mut same_fc = store(&[("ATC_RAT_RLL_P", 0.135_000_005_364_418_03), ("WPNAV_SPEED", 500.0), ("BATT_LOW_VOLT", 10.5)]);
        let c = import(&mut same_fc, &text);
        assert_eq!((c.staged, c.unchanged, c.unknown), (0, 3, 0));

        let mut other = store(&[("ATC_RAT_RLL_P", 0.2), ("WPNAV_SPEED", 400.0), ("BATT_LOW_VOLT", 10.5)]);
        let c = import(&mut other, &text);
        assert_eq!((c.staged, c.unchanged, c.unknown), (2, 1, 0));
        assert!(same(other.staged_value("ATC_RAT_RLL_P").unwrap(), 0.135));
        assert_eq!(other.staged_value("WPNAV_SPEED"), Some(500.0));
    }

    #[test]
    fn export_names_are_timestamped_and_safe() {
        assert_eq!(export_name("bench drone", 0), "bench_drone-19700101-000000.param");
        assert_eq!(stamp(951_782_400 + 3_723), "20000229-010203");
        assert_eq!(export_name("", 1_792_454_400), "ados-20261020-000000.param");
    }
}
