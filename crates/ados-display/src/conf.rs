//! `/etc/ados/display.conf` parsing.
//!
//! The file is a simple `key=value` block the LCD-overlay installer writes
//! (framebuffer_path, framebuffer_name_expected, display_id, rotation, ...).
//! Ports the parser shared by `display_conf.py` and the framebuffer probe in
//! `renderers/framebuffer.py`, plus `read_rotation` (0/90/180/270, defaulting
//! to 0 on any malformed or out-of-range value).

use std::collections::BTreeMap;
use std::path::Path;

/// Canonical config path (`core/paths.py` `DISPLAY_CONF_PATH`).
pub const DISPLAY_CONF_PATH: &str = "/etc/ados/display.conf";

/// The legal rotation values, in degrees.
pub const ALLOWED_ROTATIONS: [i32; 4] = [0, 90, 180, 270];

/// Parse a `key=value` config blob. Blank lines, `#` comments, and lines with
/// no `=` are skipped; keys and values are trimmed. Returns an empty map when
/// the file is missing or unreadable (the caller treats that as "no config").
pub fn parse(path: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    out.extend(parse_str(&text));
    out
}

/// Parse the config body from an in-memory string (the testable core).
pub fn parse_str(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || !line.contains('=') {
            continue;
        }
        let (k, v) = line.split_once('=').expect("contains '=' checked above");
        out.insert(k.trim().to_string(), v.trim().to_string());
    }
    out
}

/// Read the `rotation` key from `path`, normalized to 0/90/180/270. Returns 0
/// when the key is absent, non-numeric, or out of range.
pub fn read_rotation(path: &Path) -> i32 {
    rotation_from_blob(&parse(path))
}

/// Resolve the rotation from an already-parsed blob (the testable core).
pub fn rotation_from_blob(blob: &BTreeMap<String, String>) -> i32 {
    let Some(raw) = blob.get("rotation") else {
        return 0;
    };
    let Ok(value) = raw.parse::<i32>() else {
        return 0;
    };
    if ALLOWED_ROTATIONS.contains(&value) {
        value
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_key_value_pairs_and_skips_comments_blanks() {
        let body = "\
# a comment
framebuffer_path=/dev/fb1
   framebuffer_name_expected = fb_ili9486

display_id=lcd35
not-a-pair-line
rotation=90
";
        let m = parse_str(body);
        assert_eq!(m.get("framebuffer_path").unwrap(), "/dev/fb1");
        // Whitespace around key and value is trimmed.
        assert_eq!(m.get("framebuffer_name_expected").unwrap(), "fb_ili9486");
        assert_eq!(m.get("display_id").unwrap(), "lcd35");
        assert_eq!(m.get("rotation").unwrap(), "90");
        // The malformed line is dropped, not inserted as a key.
        assert!(!m.contains_key("not-a-pair-line"));
        assert_eq!(m.len(), 4);
    }

    #[test]
    fn value_with_embedded_equals_keeps_the_remainder() {
        // split_once on the first '=' keeps "b=c" as the value.
        let m = parse_str("key=a=b=c\n");
        assert_eq!(m.get("key").unwrap(), "a=b=c");
    }

    #[test]
    fn read_rotation_valid_values() {
        for v in ALLOWED_ROTATIONS {
            let m = parse_str(&format!("rotation={v}\n"));
            assert_eq!(rotation_from_blob(&m), v);
        }
    }

    #[test]
    fn read_rotation_defaults_to_zero_on_bad_input() {
        // Missing key.
        assert_eq!(rotation_from_blob(&parse_str("display_id=x\n")), 0);
        // Non-numeric.
        assert_eq!(rotation_from_blob(&parse_str("rotation=portrait\n")), 0);
        // Out of range.
        assert_eq!(rotation_from_blob(&parse_str("rotation=45\n")), 0);
        assert_eq!(rotation_from_blob(&parse_str("rotation=360\n")), 0);
    }

    #[test]
    fn parse_missing_file_is_empty() {
        let m = parse(Path::new("/nonexistent/display.conf"));
        assert!(m.is_empty());
        assert_eq!(rotation_from_blob(&m), 0);
    }

    #[test]
    fn read_rotation_from_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("display.conf");
        std::fs::write(&path, "framebuffer_path=/dev/fb0\nrotation=270\n").unwrap();
        assert_eq!(read_rotation(&path), 270);
    }
}
