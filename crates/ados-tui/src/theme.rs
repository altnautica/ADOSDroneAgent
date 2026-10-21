//! Colour themes for the terminal UI.
//!
//! The palettes are generated from the brand design tokens
//! (`palette_generated.rs`): one [`TuiPalette`] per selectable theme, each tone
//! carrying its 24-bit RGB value and the nearest xterm-256 index. A [`Theme`]
//! pairs a palette with the colour tier the terminal can render, so every role
//! falls back RGB → 256-index → a basic named colour → the terminal default.
//!
//! The tier is resolved once from the environment. Resolution and colour
//! picking are pure functions so the fallback ladder is unit-tested without
//! touching the process environment.

use ratatui::style::{Color, Modifier, Style};

#[path = "palette_generated.rs"]
mod palette_generated;

pub use palette_generated::{Tone, TuiPalette};

/// How much colour the terminal can render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// 24-bit RGB (`COLORTERM=truecolor`).
    Truecolor,
    /// 256-colour indexed (`TERM=*-256color`).
    Ansi256,
    /// Basic 16-colour (named foreground).
    Basic,
    /// No colour: `NO_COLOR`, or `TERM=dumb`.
    None,
}

/// Resolve the colour tier from the process environment.
pub fn env_tier() -> Tier {
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let term = std::env::var("TERM").unwrap_or_default();
    let colorterm = std::env::var("COLORTERM").unwrap_or_default();
    resolve_tier(no_color, &term, &colorterm)
}

/// Pure tier resolution (tested): `NO_COLOR` / `TERM=dumb` → plain;
/// `COLORTERM=truecolor|24bit` → truecolor; a `*-256color` `TERM` → 256; else
/// basic.
pub fn resolve_tier(no_color: bool, term: &str, colorterm: &str) -> Tier {
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

/// Pure colour pick (tested): the RGB value on truecolor, the 256-index on a
/// 256-colour terminal, the basic named colour otherwise, and the terminal
/// default (`Reset`) when there is no colour.
fn color_for(tier: Tier, tone: Tone, basic: Color) -> Color {
    match tier {
        Tier::Truecolor => Color::Rgb(tone.rgb.0, tone.rgb.1, tone.rgb.2),
        Tier::Ansi256 => Color::Indexed(tone.idx),
        Tier::Basic => basic,
        Tier::None => Color::Reset,
    }
}

/// A selectable theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeId {
    Amber,
    BrandDark,
    BrandLight,
    Nvg,
}

impl ThemeId {
    /// Every theme, in the order the settings value cycles.
    pub const ALL: [ThemeId; 4] = [
        ThemeId::Amber,
        ThemeId::BrandDark,
        ThemeId::BrandLight,
        ThemeId::Nvg,
    ];

    /// The name stored in the preferences file.
    pub fn name(self) -> &'static str {
        match self {
            ThemeId::Amber => "amber",
            ThemeId::BrandDark => "brand-dark",
            ThemeId::BrandLight => "brand-light",
            ThemeId::Nvg => "nvg",
        }
    }

    /// The theme a preferences value names; unknown names are amber.
    pub fn from_name(name: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|t| t.name() == name)
            .unwrap_or(ThemeId::Amber)
    }

    fn palette(self) -> &'static TuiPalette {
        match self {
            ThemeId::Amber => &palette_generated::AMBER,
            ThemeId::BrandDark => &palette_generated::BRAND_DARK,
            ThemeId::BrandLight => &palette_generated::BRAND_LIGHT,
            ThemeId::Nvg => &palette_generated::NVG,
        }
    }
}

/// The colours one screen is drawn with.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub tier: Tier,
    palette: &'static TuiPalette,
    pub color_blind: bool,
}

impl Theme {
    pub fn new(tier: Tier, id: ThemeId, color_blind: bool) -> Self {
        Self {
            tier,
            palette: id.palette(),
            color_blind,
        }
    }

    /// The screen background, only on tiers that can render one (truecolor and
    /// 256); basic and plain terminals keep their own background.
    pub fn canvas(&self) -> Option<Color> {
        match self.tier {
            Tier::Truecolor | Tier::Ansi256 => {
                Some(color_for(self.tier, self.palette.canvas, Color::Reset))
            }
            _ => None,
        }
    }

    /// The canvas colour effects fade from/to; the terminal default when the
    /// tier paints no background.
    pub fn canvas_or_reset(&self) -> Color {
        self.canvas().unwrap_or(Color::Reset)
    }

    pub fn border(&self) -> Color {
        color_for(self.tier, self.palette.border, Color::DarkGray)
    }
    /// Primary text: headings and values.
    pub fn fg(&self) -> Color {
        color_for(self.tier, self.palette.fg_primary, Color::Reset)
    }
    /// Secondary text: labels and dim lines.
    pub fn muted(&self) -> Color {
        color_for(self.tier, self.palette.fg_secondary, Color::DarkGray)
    }
    pub fn accent(&self) -> Color {
        color_for(self.tier, self.palette.accent, Color::Yellow)
    }
    /// Text drawn on an accent (or status) fill.
    pub fn on_accent(&self) -> Color {
        color_for(self.tier, self.palette.on_accent, Color::Black)
    }
    /// Success; drawn with the info colour in colour-blind mode.
    pub fn success(&self) -> Color {
        if self.color_blind {
            self.info()
        } else {
            color_for(self.tier, self.palette.success, Color::Green)
        }
    }
    pub fn warning(&self) -> Color {
        color_for(self.tier, self.palette.warning, Color::Yellow)
    }
    pub fn error(&self) -> Color {
        color_for(self.tier, self.palette.error, Color::Red)
    }
    pub fn info(&self) -> Color {
        color_for(self.tier, self.palette.info, Color::Blue)
    }

    // --- styles -----------------------------------------------------------

    /// Dim label text.
    pub fn dim(&self) -> Style {
        Style::default().fg(self.muted())
    }
    /// Bright, bold value text.
    pub fn bright(&self) -> Style {
        Style::default().fg(self.fg()).add_modifier(Modifier::BOLD)
    }
    /// Primary text without bold.
    pub fn text(&self) -> Style {
        Style::default().fg(self.fg())
    }
    /// Accent text.
    pub fn accented(&self) -> Style {
        Style::default().fg(self.accent())
    }
    /// Bold accent text.
    pub fn accent_bold(&self) -> Style {
        Style::default()
            .fg(self.accent())
            .add_modifier(Modifier::BOLD)
    }
    /// Text on an accent fill (active tab, chips).
    pub fn on_accent_fill(&self) -> Style {
        Style::default().fg(self.on_accent()).bg(self.accent())
    }
    /// A chip filled with `color`.
    pub fn chip(&self, color: Color) -> Style {
        Style::default().fg(self.on_accent()).bg(color)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_tier_reads_the_ladder() {
        assert_eq!(
            resolve_tier(true, "xterm-256color", "truecolor"),
            Tier::None
        );
        assert_eq!(resolve_tier(false, "dumb", "truecolor"), Tier::None);
        assert_eq!(resolve_tier(false, "xterm", "truecolor"), Tier::Truecolor);
        assert_eq!(resolve_tier(false, "xterm", "24bit"), Tier::Truecolor);
        assert_eq!(resolve_tier(false, "xterm-256color", ""), Tier::Ansi256);
        assert_eq!(resolve_tier(false, "xterm", ""), Tier::Basic);
    }

    #[test]
    fn color_for_falls_back_per_tier() {
        let amber = palette_generated::AMBER.accent;
        assert_eq!(
            color_for(Tier::Truecolor, amber, Color::Yellow),
            Color::Rgb(235, 193, 87)
        );
        assert_eq!(
            color_for(Tier::Ansi256, amber, Color::Yellow),
            Color::Indexed(179)
        );
        assert_eq!(color_for(Tier::Basic, amber, Color::Yellow), Color::Yellow);
        assert_eq!(color_for(Tier::None, amber, Color::Yellow), Color::Reset);
    }

    #[test]
    fn color_blind_success_is_the_info_colour() {
        let normal = Theme::new(Tier::Truecolor, ThemeId::Amber, false);
        let cb = Theme::new(Tier::Truecolor, ThemeId::Amber, true);
        assert_ne!(normal.success(), normal.info());
        assert_eq!(cb.success(), cb.info());
    }

    #[test]
    fn only_rich_tiers_paint_a_background() {
        assert!(Theme::new(Tier::Ansi256, ThemeId::Nvg, false).canvas().is_some());
        assert!(Theme::new(Tier::Basic, ThemeId::Nvg, false).canvas().is_none());
        assert_eq!(ThemeId::from_name("brand-light"), ThemeId::BrandLight);
        assert_eq!(ThemeId::from_name("nope"), ThemeId::Amber);
    }
}
