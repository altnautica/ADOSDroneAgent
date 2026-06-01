//! Color palette for the LCD dashboards.
//!
//! Two named sets — dark (default) and light — name every color a page can
//! paint. The values match the on-device theme one-for-one so a render is
//! pixel-identical regardless of which side produces it. Colors are stored as
//! `embedded_graphics` `Rgb888` so primitives can hand them straight to the
//! draw target without a per-call conversion.
//!
//! The threshold helper maps a measured value to a success / warning / error
//! color given two cut points and a direction, used by the headline numbers
//! (battery percent reads higher-is-better, CPU and temperature read
//! lower-is-better). A `None` value renders in the muted tertiary grey so the
//! operator reads "no data" rather than a misleading status color.

use embedded_graphics::pixelcolor::Rgb888;

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

/// The dark theme — a true-black ground with near-white primary text. This is
/// the default; a fresh rig with an unreadable or absent theme config falls
/// back here.
pub const DARK: Palette = Palette {
    name: ThemeName::Dark,
    bg_primary: Rgb888::new(0x00, 0x00, 0x00),
    bg_secondary: Rgb888::new(0x0A, 0x0A, 0x0A),
    bg_tertiary: Rgb888::new(0x14, 0x14, 0x14),
    text_primary: Rgb888::new(0xFA, 0xFA, 0xFA),
    text_secondary: Rgb888::new(0xA0, 0xA0, 0xA0),
    text_tertiary: Rgb888::new(0x66, 0x66, 0x66),
    accent_primary: Rgb888::new(0x3A, 0x82, 0xFF),
    accent_secondary: Rgb888::new(0xDF, 0xF1, 0x40),
    border_default: Rgb888::new(0x1A, 0x1A, 0x1A),
    border_strong: Rgb888::new(0x2A, 0x2A, 0x2A),
    status_success: Rgb888::new(0x22, 0xC5, 0x5E),
    status_warning: Rgb888::new(0xF5, 0x9E, 0x0B),
    status_error: Rgb888::new(0xEF, 0x44, 0x44),
};

/// The light theme — a white ground with near-black primary text.
pub const LIGHT: Palette = Palette {
    name: ThemeName::Light,
    bg_primary: Rgb888::new(0xFF, 0xFF, 0xFF),
    bg_secondary: Rgb888::new(0xF8, 0xF8, 0xF8),
    bg_tertiary: Rgb888::new(0xEC, 0xEC, 0xEC),
    text_primary: Rgb888::new(0x0A, 0x0A, 0x0A),
    text_secondary: Rgb888::new(0x4A, 0x4A, 0x4A),
    text_tertiary: Rgb888::new(0x8A, 0x8A, 0x8A),
    accent_primary: Rgb888::new(0x14, 0x5A, 0xE0),
    accent_secondary: Rgb888::new(0xB8, 0xCC, 0x10),
    border_default: Rgb888::new(0xE2, 0xE2, 0xE2),
    border_strong: Rgb888::new(0xC9, 0xC9, 0xC9),
    status_success: Rgb888::new(0x16, 0xA3, 0x4A),
    status_warning: Rgb888::new(0xC2, 0x6F, 0x00),
    status_error: Rgb888::new(0xC4, 0x1E, 0x3A),
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
    fn dark_primary_text_is_near_white() {
        assert_eq!(DARK.text_primary, Rgb888::new(0xFA, 0xFA, 0xFA));
        assert_eq!(DARK.bg_primary, Rgb888::new(0x00, 0x00, 0x00));
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
