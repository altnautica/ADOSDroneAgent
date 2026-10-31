//! Text formatting and measuring helpers shared by the dashboard tiles.

use crate::graphics::fonts::{FontFace, LoadedFont};

/// Group an integer with thousands separators (`1247` -> `1,247`), matching the
/// FEC counters' comma-grouped display.
pub(super) fn group_thousands(value: i64) -> String {
    let neg = value < 0;
    let digits = value.unsigned_abs().to_string();
    let mut out = String::new();
    let len = digits.len();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if neg {
        format!("-{out}")
    } else {
        out
    }
}

/// Inked width of `s` at `(face, px)`.
pub(super) fn measure(face: FontFace, px: u32, s: &str) -> i32 {
    LoadedFont::new(face, px).text_size(s).0 as i32
}

/// Largest size in `[min_px, max_px]` whose inked width of `s` fits `max_w`,
/// floored at `min_px`. Mirrors the auto-shrink the pair-code and wizard-URL
/// tiles use so a long string never overflows its column.
pub(super) fn fit_font(face: FontFace, s: &str, max_w: i32, max_px: u32, min_px: u32) -> u32 {
    let mut px = max_px;
    while px > min_px {
        if measure(face, px, s) <= max_w {
            return px;
        }
        px -= 1;
    }
    min_px
}

/// Trim `s` with a trailing ellipsis until it fits `max_w` at `(face, px)`.
pub(super) fn truncate_to_width(face: FontFace, px: u32, s: &str, max_w: i32) -> String {
    if measure(face, px, s) <= max_w {
        return s.to_string();
    }
    let mut chars: Vec<char> = s.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let candidate: String = chars.iter().collect::<String>() + "…";
        if measure(face, px, &candidate) <= max_w {
            return candidate;
        }
    }
    String::new()
}

// ── id formatting ───────────────────────────────────────────────────

/// The last `n` characters of `s`, uppercased — the short-id form the tiles
/// use for the device-id caption and the mesh id.
pub(super) fn last_n_upper(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let start = chars.len().saturating_sub(n);
    chars[start..]
        .iter()
        .collect::<String>()
        .to_ascii_uppercase()
}

/// The first `n` characters of `s`, uppercased — the truncated flight-mode form.
pub(super) fn first_n_upper(s: &str, n: usize) -> String {
    s.chars().take(n).collect::<String>().to_ascii_uppercase()
}
