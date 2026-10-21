//! Glyph presets. One table per preset so every screen draws the same glyph
//! for the same meaning, and a terminal without a Nerd Font (or without
//! Unicode at all) still gets a legible screen.

use ratatui::symbols::border;

/// The glyphs one preset draws.
#[derive(Debug)]
pub struct Symbols {
    /// Preset name as stored in the preferences file.
    pub name: &'static str,
    /// One icon per tab, in tab order. Empty strings draw the label only.
    pub tab_icons: [&'static str; 9],
    pub ok: &'static str,
    pub warn: &'static str,
    pub err: &'static str,
    pub info: &'static str,
    /// Focused-row marker.
    pub row_marker: &'static str,
    /// Selected sidebar page marker.
    pub page_marker: &'static str,
    pub staged: &'static str,
    pub non_default: &'static str,
    pub favourite: &'static str,
    pub reboot: &'static str,
    pub read_only: &'static str,
    /// Spinner animation frames.
    pub spinner: &'static [&'static str],
    /// Signal-strength bars, weakest to strongest.
    pub signal: [&'static str; 4],
    /// Panel border glyphs (rounded, or `+-|` for ASCII).
    pub border_set: border::Set<'static>,
}

const BRAILLE_SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Plain ASCII borders: `+` corners, `-` and `|` sides.
const ASCII_BORDER: border::Set<'static> = border::Set {
    top_left: "+",
    top_right: "+",
    bottom_left: "+",
    bottom_right: "+",
    vertical_left: "|",
    vertical_right: "|",
    horizontal_top: "-",
    horizontal_bottom: "-",
};

/// Nerd Font Material Design icons.
pub const NERD: Symbols = Symbols {
    name: "nerd",
    tab_icons: [
        "\u{f056e}", // md-view_dashboard
        "\u{f0434}", // md-quadcopter
        "\u{f0003}", // md-access_point
        "\u{f0567}", // md-video
        "\u{f066a}", // md-tune_vertical
        "\u{f0431}", // md-puzzle
        "\u{f09ed}", // md-text_box_outline
        "\u{f061a}", // md-chip
        "\u{f0493}", // md-cog
    ],
    ok: "✓",
    warn: "▲",
    err: "✗",
    info: "·",
    row_marker: "›",
    page_marker: "▸",
    staged: "●",
    non_default: "◆",
    favourite: "★",
    reboot: "⟳",
    read_only: "\u{f033e}", // md-lock
    spinner: BRAILLE_SPINNER,
    signal: ["▂", "▄", "▆", "█"],
    border_set: border::ROUNDED,
};

/// Plain Unicode, no private-use glyphs.
pub const UNICODE: Symbols = Symbols {
    name: "unicode",
    // `≡` for Params and `▤` for Logs: the variation-selector gear and `☰`
    // render at different widths across terminals and shift every cell after
    // them.
    tab_icons: ["◎", "✈", "≋", "▶", "≡", "◇", "▤", "▣", "⚙"],
    ok: "✓",
    warn: "▲",
    err: "✗",
    info: "·",
    row_marker: "›",
    page_marker: "▸",
    staged: "●",
    non_default: "◆",
    favourite: "★",
    reboot: "⟳",
    read_only: "🔒",
    spinner: BRAILLE_SPINNER,
    signal: ["▂", "▄", "▆", "█"],
    border_set: border::ROUNDED,
};

/// 7-bit ASCII only.
pub const ASCII: Symbols = Symbols {
    name: "ascii",
    tab_icons: ["", "", "", "", "", "", "", "", ""],
    ok: "+",
    warn: "!",
    err: "x",
    info: "-",
    row_marker: ">",
    page_marker: ">",
    staged: "*",
    non_default: "~",
    favourite: "+",
    reboot: "R",
    read_only: "ro",
    spinner: &["-", "\\", "|", "/"],
    signal: [".", "o", "O", "#"],
    border_set: ASCII_BORDER,
};

/// The preset a preferences value names; unknown names fall back to Unicode.
pub fn by_name(name: &str) -> &'static Symbols {
    match name {
        "nerd" => &NERD,
        "ascii" => &ASCII,
        _ => &UNICODE,
    }
}

impl Symbols {
    /// The spinner frame for animation tick `n`.
    pub fn spinner_frame(&self, n: usize) -> &'static str {
        self.spinner[n % self.spinner.len()]
    }

    /// Four-step signal bars for a 0–100 strength: one glyph per filled step,
    /// so stronger signals draw longer bars.
    pub fn signal_bars(&self, strength: u8) -> String {
        let steps = match strength {
            0..=24 => 1,
            25..=49 => 2,
            50..=74 => 3,
            _ => 4,
        };
        self.signal[..steps].concat()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_bars_grow_with_strength() {
        assert_eq!(UNICODE.signal_bars(10), "▂");
        assert_eq!(UNICODE.signal_bars(90), "▂▄▆█");
        assert_eq!(ASCII.signal_bars(60), ".oO");
    }

    #[test]
    fn unknown_preset_is_unicode() {
        assert_eq!(by_name("bogus").name, "unicode");
        assert_eq!(by_name("ascii").name, "ascii");
    }
}
