//! Color palette for the LCD dashboards.
//!
//! Two named sets — dark (default) and light — name every color a page can
//! paint. The values come from the generated brand tokens
//! (`palette_generated.rs`: brand-dark and brand-light roles), so the panels
//! share one palette with every other ADOS surface. Colors are stored as
//! `embedded_graphics` `Rgb888` so primitives can hand them straight to the
//! draw target without a per-call conversion.
//!
//! The threshold helper maps a measured value to a success / warning / error
//! color given two cut points and a direction, used by the headline numbers
//! (battery percent reads higher-is-better, CPU and temperature read
//! lower-is-better). A `None` value renders in the muted tertiary grey so the
//! operator reads "no data" rather than a misleading status color.

use embedded_graphics::pixelcolor::Rgb888;

#[path = "palette_generated.rs"]
mod palette_generated;

use palette_generated as tokens;

/// A generated `(r, g, b)` token as an `Rgb888`.
const fn rgb(c: (u8, u8, u8)) -> Rgb888 {
    Rgb888::new(c.0, c.1, c.2)
}

/// Which theme a palette represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeName {
    Dark,
    Light,
}

/// Every named color a page paints. Field names mirror the published design
/// tokens so a color shipped on the tokens has an obvious home here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub name: ThemeName,
    pub bg_primary: Rgb888,
    pub bg_secondary: Rgb888,
    pub bg_tertiary: Rgb888,
    pub text_primary: Rgb888,
    pub text_secondary: Rgb888,
    pub text_tertiary: Rgb888,
    pub accent_primary: Rgb888,
    pub accent_secondary: Rgb888,
    pub border_default: Rgb888,
    pub border_strong: Rgb888,
    pub status_success: Rgb888,
    pub status_warning: Rgb888,
    pub status_error: Rgb888,
}

/// Whether a higher or a lower measured value is the good direction when
/// mapping a number to a status color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThresholdDirection {
    HigherIsBetter,
    LowerIsBetter,
}

/// The dark theme (brand-dark) — a navy ground with near-white primary text.
/// This is the default; a fresh rig with an unreadable or absent theme config
/// falls back here.
pub const DARK: Palette = Palette {
    name: ThemeName::Dark,
    bg_primary: rgb(tokens::BRAND_DARK_BG_CANVAS),
    bg_secondary: rgb(tokens::BRAND_DARK_BG_SURFACE),
    bg_tertiary: rgb(tokens::BRAND_DARK_BG_HOVER),
    text_primary: rgb(tokens::BRAND_DARK_FG_PRIMARY),
    text_secondary: rgb(tokens::BRAND_DARK_FG_SECONDARY),
    text_tertiary: rgb(tokens::BRAND_DARK_FG_TERTIARY),
    accent_primary: rgb(tokens::BRAND_DARK_ACCENT_PRIMARY),
    accent_secondary: rgb(tokens::BRAND_DARK_ACCENT_SECONDARY),
    border_default: rgb(tokens::BRAND_DARK_BORDER_DEFAULT),
    border_strong: rgb(tokens::BRAND_DARK_BORDER_STRONG),
    status_success: rgb(tokens::BRAND_DARK_STATUS_SUCCESS),
    status_warning: rgb(tokens::BRAND_DARK_STATUS_WARNING),
    status_error: rgb(tokens::BRAND_DARK_STATUS_ERROR),
};

/// The light theme (brand-light) — a white ground with near-black primary text.
pub const LIGHT: Palette = Palette {
    name: ThemeName::Light,
    bg_primary: rgb(tokens::BRAND_LIGHT_BG_CANVAS),
    bg_secondary: rgb(tokens::BRAND_LIGHT_BG_SURFACE),
    bg_tertiary: rgb(tokens::BRAND_LIGHT_BG_HOVER),
    text_primary: rgb(tokens::BRAND_LIGHT_FG_PRIMARY),
    text_secondary: rgb(tokens::BRAND_LIGHT_FG_SECONDARY),
    text_tertiary: rgb(tokens::BRAND_LIGHT_FG_TERTIARY),
    accent_primary: rgb(tokens::BRAND_LIGHT_ACCENT_PRIMARY),
    accent_secondary: rgb(tokens::BRAND_LIGHT_ACCENT_SECONDARY),
    border_default: rgb(tokens::BRAND_LIGHT_BORDER_DEFAULT),
    border_strong: rgb(tokens::BRAND_LIGHT_BORDER_STRONG),
    status_success: rgb(tokens::BRAND_LIGHT_STATUS_SUCCESS),
    status_warning: rgb(tokens::BRAND_LIGHT_STATUS_WARNING),
    status_error: rgb(tokens::BRAND_LIGHT_STATUS_ERROR),
};

/// Return the palette for a theme name string. An unknown name resolves to
/// [`DARK`]; the caller decides whether to log the fallback.
pub fn get_palette(name: &str) -> Palette {
    match name {
        "light" => LIGHT,
        "dark" => DARK,
        _ => DARK,
    }
}

impl Palette {
    /// Map a measured value to success / warning / error based on two cut
    /// points and a direction.
    ///
    /// For [`ThresholdDirection::HigherIsBetter`]: at or above `success_at` is
    /// success, at or above `warning_at` is warning, otherwise error. For
    /// [`ThresholdDirection::LowerIsBetter`] the comparison flips: at or below
    /// `success_at` is success, at or below `warning_at` is warning, otherwise
    /// error. A `None` value returns the muted tertiary text color.
    pub fn threshold_color(
        &self,
        value: Option<f64>,
        success_at: f64,
        warning_at: f64,
        direction: ThresholdDirection,
    ) -> Rgb888 {
        let v = match value {
            Some(v) => v,
            None => return self.text_tertiary,
        };
        match direction {
            ThresholdDirection::HigherIsBetter => {
                if v >= success_at {
                    self.status_success
                } else if v >= warning_at {
                    self.status_warning
                } else {
                    self.status_error
                }
            }
            ThresholdDirection::LowerIsBetter => {
                if v <= success_at {
                    self.status_success
                } else if v <= warning_at {
                    self.status_warning
                } else {
                    self.status_error
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_is_the_unknown_name_fallback() {
        assert_eq!(get_palette("dark"), DARK);
        assert_eq!(get_palette("light"), LIGHT);
        assert_eq!(get_palette("chartreuse"), DARK);
        assert_eq!(get_palette(""), DARK);
    }

    #[test]
    fn dark_is_the_brand_navy_ground() {
        assert_eq!(DARK.text_primary, Rgb888::new(0xF8, 0xFA, 0xFC));
        assert_eq!(DARK.bg_primary, Rgb888::new(0x0A, 0x0A, 0x0F));
        assert_eq!(DARK.accent_primary, Rgb888::new(0x3A, 0x82, 0xFF));
    }

    #[test]
    fn threshold_none_is_tertiary() {
        let c = DARK.threshold_color(None, 70.0, 85.0, ThresholdDirection::HigherIsBetter);
        assert_eq!(c, DARK.text_tertiary);
    }

    #[test]
    fn threshold_higher_is_better_bands() {
        // Battery-style: high is good.
        let p = DARK;
        let dir = ThresholdDirection::HigherIsBetter;
        assert_eq!(
            p.threshold_color(Some(90.0), 50.0, 20.0, dir),
            p.status_success
        );
        assert_eq!(
            p.threshold_color(Some(50.0), 50.0, 20.0, dir),
            p.status_success
        );
        assert_eq!(
            p.threshold_color(Some(30.0), 50.0, 20.0, dir),
            p.status_warning
        );
        assert_eq!(
            p.threshold_color(Some(20.0), 50.0, 20.0, dir),
            p.status_warning
        );
        assert_eq!(
            p.threshold_color(Some(10.0), 50.0, 20.0, dir),
            p.status_error
        );
    }

    #[test]
    fn threshold_lower_is_better_bands() {
        // CPU / temperature-style: low is good.
        let p = DARK;
        let dir = ThresholdDirection::LowerIsBetter;
        assert_eq!(
            p.threshold_color(Some(40.0), 70.0, 85.0, dir),
            p.status_success
        );
        assert_eq!(
            p.threshold_color(Some(70.0), 70.0, 85.0, dir),
            p.status_success
        );
        assert_eq!(
            p.threshold_color(Some(80.0), 70.0, 85.0, dir),
            p.status_warning
        );
        assert_eq!(
            p.threshold_color(Some(85.0), 70.0, 85.0, dir),
            p.status_warning
        );
        assert_eq!(
            p.threshold_color(Some(95.0), 70.0, 85.0, dir),
            p.status_error
        );
    }
}
