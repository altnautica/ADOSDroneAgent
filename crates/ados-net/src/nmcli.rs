//! Canonical `nmcli -t` (terse) output parser.
//!
//! `nmcli -t` emits colon-separated fields, escaping a literal colon as `\:`
//! and a literal backslash as `\\`. The Python ethernet and wifi-client
//! managers each carried their own copy of this split; this is the single
//! shared implementation both Rust managers call.

/// Split one terse line into fields, honoring `\:` and `\\` escapes. An odd
/// trailing backslash is treated as a literal backslash (matches the Python
/// `i + 1 < len(line)` guard).
pub fn parse_terse_line(line: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut buf = String::new();
    let bytes: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        if ch == '\\' && i + 1 < bytes.len() {
            buf.push(bytes[i + 1]);
            i += 2;
            continue;
        }
        if ch == ':' {
            parts.push(std::mem::take(&mut buf));
            i += 1;
            continue;
        }
        buf.push(ch);
        i += 1;
    }
    parts.push(buf);
    parts
}

/// Parse multi-line terse output into rows, skipping blank lines and keeping
/// only rows with at least `fields` columns (each truncated to `fields`).
/// Mirrors the Python `_parse_nmcli_terse(text, fields)`.
pub fn parse_terse(text: &str, fields: usize) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parts = parse_terse_line(line);
        if parts.len() >= fields {
            rows.push(parts.into_iter().take(fields).collect());
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_colon_split() {
        assert_eq!(
            parse_terse_line("Wired connection 1:802-3-ethernet:eth0"),
            vec!["Wired connection 1", "802-3-ethernet", "eth0"]
        );
    }

    #[test]
    fn escaped_colon_and_backslash() {
        // A BSSID with escaped colons stays one field.
        assert_eq!(
            parse_terse_line(r"MyAP:AA\:BB\:CC:70"),
            vec!["MyAP", "AA:BB:CC", "70"]
        );
        // Escaped backslash becomes a single literal backslash.
        assert_eq!(parse_terse_line(r"a\\b:c"), vec![r"a\b", "c"]);
        // Trailing lone backslash is a literal backslash.
        assert_eq!(parse_terse_line(r"x\"), vec![r"x\"]);
    }

    #[test]
    fn empty_fields_preserved() {
        // A missing DEVICE column yields an empty field, not a dropped one.
        assert_eq!(
            parse_terse_line("name:802-3-ethernet:"),
            vec!["name", "802-3-ethernet", ""]
        );
    }

    #[test]
    fn multiline_filters_short_rows_and_truncates() {
        let text = "a:b:c\n\n  \nx:y\np:q:r:s\n";
        // fields=3 keeps the 3- and 4-col rows (truncated to 3), drops "x:y".
        assert_eq!(
            parse_terse(text, 3),
            vec![vec!["a", "b", "c"], vec!["p", "q", "r"],]
        );
    }
}
