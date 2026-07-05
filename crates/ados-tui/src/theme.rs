//! Amber-on-charcoal palette for the cockpit.
//!
//! Mirrors the installer/wizard house style (the hand-rolled
//! `crates/ados-installer/src/ui/theme.rs`) but expressed as `ratatui` colors:
//! a single amber accent, muted grey body text, near-white headings, and
//! semantic success / warning / danger. ratatui emits `Color::Rgb` as 24-bit
//! truecolor; on a terminal that cannot show it (or with `NO_COLOR`), each
//! color falls back to its nearest 256-index and then a basic named color, so
//! the cockpit stays legible everywhere.
//!
//! The tier is resolved once from the environment and cached. The resolution
//! and color-picking are split into pure functions so the fallback ladder is
//! unit-tested without touching the process environment.

use std::sync::OnceLock;

use ratatui::style::Color;

/// How much color the terminal can render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    /// 24-bit RGB (`COLORTERM=truecolor`).
    Truecolor,
    /// 256-color indexed (`TERM=*-256color`).
    Ansi256,
    /// Basic 16-color (named foreground).
    Basic,
    /// No color: `NO_COLOR`, or `TERM=dumb`.
    None,
}

// --- palette (matches the installer's amber-on-charcoal values) -------------

/// Amber accent (selected / acted-on / the word-mark).
const AMBER: (u8, u8, u8) = (235, 193, 87);
/// Muted grey body / label text.
const MUTED: (u8, u8, u8) = (138, 138, 138);
/// Near-white heading / value text.
const HEADING: (u8, u8, u8) = (233, 233, 233);
/// Charcoal screen background.
const CHARCOAL: (u8, u8, u8) = (31, 31, 31);
/// Dark text drawn on top of an amber chip / gauge.
const ON_AMBER: (u8, u8, u8) = (26, 26, 26);
/// Success green.
const SUCCESS: (u8, u8, u8) = (121, 200, 121);
/// Warning yellow.
const WARNING: (u8, u8, u8) = (227, 199, 102);
/// Destructive salmon red.
const DANGER: (u8, u8, u8) = (224, 108, 90);

static TIER: OnceLock<Tier> = OnceLock::new();

/// Resolve the color tier from the environment, once, and cache it.
fn tier() -> Tier {
    *TIER.get_or_init(|| {
        let no_color = std::env::var_os("NO_COLOR").is_some();
        let term = std::env::var("TERM").unwrap_or_default();
        let colorterm = std::env::var("COLORTERM").unwrap_or_default();
        resolve_tier(no_color, &term, &colorterm)
    })
}

/// Pure tier resolution (tested): `NO_COLOR` / `TERM=dumb` → plain;
/// `COLORTERM=truecolor|24bit` → truecolor; a `*-256color` `TERM` → 256; else
/// basic.
fn resolve_tier(no_color: bool, term: &str, colorterm: &str) -> Tier {
    if no_color || term == "dumb" {
        return Tier::None;
    }
    let colorterm = colorterm.to_ascii_lowercase();
    if colorterm.contains("truecolor") || colorterm.contains("24bit") {
        Tier::Truecolor
    } else if term.contains("256color") {
        Tier::Ansi256
    } else {
        Tier::Basic
    }
}

/// Pure color pick (tested): the RGB value on truecolor, the 256-index on a
/// 256-color terminal, the basic named color otherwise, and the terminal
/// default (`Reset`) when there is no color.
fn color_for(tier: Tier, rgb: (u8, u8, u8), idx: u8, basic: Color) -> Color {
    match tier {
        Tier::Truecolor => Color::Rgb(rgb.0, rgb.1, rgb.2),
        Tier::Ansi256 => Color::Indexed(idx),
        Tier::Basic => basic,
        Tier::None => Color::Reset,
    }
}

/// The amber accent.
pub fn accent() -> Color {
    color_for(tier(), AMBER, 179, Color::Yellow)
}
/// Muted grey for labels, dim text, and inactive dots.
pub fn muted() -> Color {
    color_for(tier(), MUTED, 245, Color::DarkGray)
}
/// Near-white for headings and bright values.
pub fn heading() -> Color {
    color_for(tier(), HEADING, 254, Color::White)
}
/// Dark text on an amber chip / gauge.
pub fn on_amber() -> Color {
    color_for(tier(), ON_AMBER, 235, Color::Black)
}
/// Success green.
pub fn success() -> Color {
    color_for(tier(), SUCCESS, 114, Color::Green)
}
/// Warning yellow.
pub fn warning() -> Color {
    color_for(tier(), WARNING, 179, Color::Yellow)
}
/// Destructive salmon red.
pub fn danger() -> Color {
    color_for(tier(), DANGER, 209, Color::Red)
}

/// The charcoal screen background, painted once per frame — but only on tiers
/// that can render a real background (truecolor / 256). On basic and plain
/// terminals this returns `None` so the terminal keeps its own background.
pub fn background() -> Option<Color> {
    match tier() {
        Tier::Truecolor => Some(Color::Rgb(CHARCOAL.0, CHARCOAL.1, CHARCOAL.2)),
        Tier::Ansi256 => Some(Color::Indexed(234)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_tier_reads_the_ladder() {
        assert_eq!(resolve_tier(true, "xterm-256color", "truecolor"), Tier::None);
        assert_eq!(resolve_tier(false, "dumb", "truecolor"), Tier::None);
        assert_eq!(resolve_tier(false, "xterm", "truecolor"), Tier::Truecolor);
        assert_eq!(resolve_tier(false, "xterm", "24bit"), Tier::Truecolor);
        assert_eq!(resolve_tier(false, "xterm-256color", ""), Tier::Ansi256);
        assert_eq!(resolve_tier(false, "xterm", ""), Tier::Basic);
    }

    #[test]
    fn color_for_falls_back_per_tier() {
        assert_eq!(
            color_for(Tier::Truecolor, AMBER, 179, Color::Yellow),
            Color::Rgb(235, 193, 87)
        );
        assert_eq!(
            color_for(Tier::Ansi256, AMBER, 179, Color::Yellow),
            Color::Indexed(179)
        );
        assert_eq!(
            color_for(Tier::Basic, AMBER, 179, Color::Yellow),
            Color::Yellow
        );
        assert_eq!(
            color_for(Tier::None, AMBER, 179, Color::Yellow),
            Color::Reset
        );
    }
}
