//! Color + glyph theme for the rich renderer, with capability tiers.
//!
//! Restrained-premium palette: green/red + one cyan accent + dim. Plain Unicode
//! glyphs (✓ ✗ • braille spinner ╭╮╰╯─│) with an ASCII fallback tier for
//! `--ascii`, non-UTF-8 locales, and basic terminals. `NO_COLOR` is honored
//! first; color is gated per call so a monochrome terminal still gets the full
//! layout. Width math elsewhere counts `chars()` — every glyph here is one
//! display column.

use ratatui::crossterm::style::Stylize;

/// The braille spinner frames (the modern gold standard).
const SPIN_UNICODE: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// ASCII spinner fallback.
const SPIN_ASCII: &[&str] = &["-", "\\", "|", "/"];

/// Box-drawing glyphs (rounded) and their ASCII fallbacks.
pub struct BoxChars {
    pub tl: &'static str,
    pub tr: &'static str,
    pub bl: &'static str,
    pub br: &'static str,
    pub h: &'static str,
    pub v: &'static str,
}

/// Resolved color + glyph capability for this run.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// Emit SGR color codes.
    pub color: bool,
    /// Use the ASCII glyph tier (no Unicode box/spinner/marks).
    pub ascii: bool,
}

impl Theme {
    /// Resolve from the flags + environment. `NO_COLOR` (any value) or
    /// `--no-color` disables color; `--ascii` or a non-UTF-8 locale selects the
    /// ASCII tier.
    pub fn detect(no_color_flag: bool, ascii_flag: bool) -> Theme {
        let no_color = std::env::var_os("NO_COLOR").is_some() || no_color_flag;
        Theme {
            color: !no_color,
            ascii: ascii_flag || !locale_is_utf8(),
        }
    }

    // --- color helpers (gated; return plain text when color is off) ---

    pub fn ok(&self, s: &str) -> String {
        if self.color {
            s.green().to_string()
        } else {
            s.to_string()
        }
    }
    pub fn fail(&self, s: &str) -> String {
        if self.color {
            s.red().to_string()
        } else {
            s.to_string()
        }
    }
    pub fn accent(&self, s: &str) -> String {
        if self.color {
            s.cyan().to_string()
        } else {
            s.to_string()
        }
    }
    pub fn warn(&self, s: &str) -> String {
        if self.color {
            s.yellow().to_string()
        } else {
            s.to_string()
        }
    }
    pub fn dim(&self, s: &str) -> String {
        if self.color {
            s.dark_grey().to_string()
        } else {
            s.to_string()
        }
    }
    pub fn bold(&self, s: &str) -> String {
        if self.color {
            s.bold().to_string()
        } else {
            s.to_string()
        }
    }

    // --- glyphs ---

    /// The current spinner frame glyph.
    pub fn spinner(&self, frame: usize) -> &'static str {
        if self.ascii {
            SPIN_ASCII[frame % SPIN_ASCII.len()]
        } else {
            SPIN_UNICODE[frame % SPIN_UNICODE.len()]
        }
    }
    pub fn glyph_ok(&self) -> &'static str {
        if self.ascii {
            "+"
        } else {
            "✓"
        }
    }
    pub fn glyph_fail(&self) -> &'static str {
        if self.ascii {
            "x"
        } else {
            "✗"
        }
    }
    pub fn glyph_pending(&self) -> &'static str {
        if self.ascii {
            "."
        } else {
            "•"
        }
    }

    /// The box-drawing set (rounded Unicode, or ASCII).
    pub fn box_chars(&self) -> BoxChars {
        if self.ascii {
            BoxChars {
                tl: "+",
                tr: "+",
                bl: "+",
                br: "+",
                h: "-",
                v: "|",
            }
        } else {
            BoxChars {
                tl: "╭",
                tr: "╮",
                bl: "╰",
                br: "╯",
                h: "─",
                v: "│",
            }
        }
    }
}

/// True when the locale env vars indicate UTF-8 (or are unset — the modern
/// default). A `C`/`POSIX` locale selects the ASCII tier.
fn locale_is_utf8() -> bool {
    // POSIX precedence: LC_ALL wins, then LC_CTYPE, then LANG. The first set,
    // non-empty value decides; nothing set → assume a modern UTF-8 terminal.
    for key in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Ok(v) = std::env::var(key) {
            if v.is_empty() {
                continue;
            }
            let v = v.to_ascii_lowercase();
            return v.contains("utf-8") || v.contains("utf8");
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_disables_sgr() {
        let t = Theme {
            color: false,
            ascii: false,
        };
        assert_eq!(t.ok("✓"), "✓");
        assert_eq!(t.dim("x"), "x");
    }

    #[test]
    fn ascii_tier_uses_ascii_glyphs() {
        let t = Theme {
            color: false,
            ascii: true,
        };
        assert_eq!(t.glyph_ok(), "+");
        assert_eq!(t.glyph_fail(), "x");
        assert_eq!(t.spinner(0), "-");
        assert_eq!(t.box_chars().h, "-");
    }

    #[test]
    fn unicode_glyphs_are_single_column() {
        let t = Theme {
            color: false,
            ascii: false,
        };
        for g in [
            t.glyph_ok(),
            t.glyph_fail(),
            t.glyph_pending(),
            t.spinner(2),
        ] {
            assert_eq!(g.chars().count(), 1, "glyph {g:?} must be one column");
        }
    }
}
