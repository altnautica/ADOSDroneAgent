//! The readouts beside the chip: the link-loss action and countdown (the
//! shared `link-loss.ts`) and the height above ground with its source mark
//! (`chip-inputs.ts` `bottomRowAgl`).

use crate::model::autonomy::{Follow, Pilot};
use crate::model::spec_text::{has_spec_text, spec_text};

/// The text of a link-loss action, or a dash for none or one the spec does
/// not name.
pub fn link_loss_action_text(action: Option<&str>) -> String {
    match action {
        Some(a) if has_spec_text(&format!("pilot.link_loss_action.{a}")) => {
            spec_text(&format!("pilot.link_loss_action.{a}"), &[])
        }
        _ => "—".to_string(),
    }
}

/// The countdown line (`Link lost: Return home in 4 s`), or `None` with no
/// countdown running.
pub fn link_loss_line(p: &Pilot) -> Option<String> {
    let in_s = p.link_loss_in_s?;
    let action = link_loss_action_text(p.link_loss_action.as_deref());
    let secs = (in_s.ceil() as i64).to_string();
    Some(spec_text(
        "flow.link_loss.countdown",
        &[("action", &action), ("in_s", &secs)],
    ))
}

/// Where a height above ground came from. `Home` (the EKF height above home)
/// and `Baro` are not ground measurements and are marked beside the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AglSource {
    Rangefinder,
    Home,
    Baro,
}

impl AglSource {
    /// The spec's label: `AGL`, `HOME` or `BARO`.
    pub fn text(self) -> String {
        spec_text(
            match self {
                Self::Rangefinder => "agl.source.rangefinder",
                Self::Home => "agl.source.home",
                Self::Baro => "agl.source.baro",
            },
            &[],
        )
    }

    /// Whether the value is not a ground measurement.
    pub fn marked(self) -> bool {
        self != Self::Rangefinder
    }
}

/// Height above ground: the pilot kind's `agl_m` (a fresh rangefinder, else
/// the EKF height above home), else the follow kind's (a rangefinder, or the
/// barometer). The `bool` says whether it came from the follow kind.
pub fn bottom_row_agl(
    pilot: Option<&Pilot>,
    follow: Option<&Follow>,
) -> Option<(f64, AglSource, bool)> {
    if let Some((m, src)) = pilot.and_then(|p| p.agl_m.map(|m| (m, p.agl_source.as_deref()))) {
        let src = if src == Some("rangefinder") {
            AglSource::Rangefinder
        } else {
            AglSource::Home
        };
        return Some((m, src, false));
    }
    let f = follow?;
    let m = f.agl_m?;
    let src = if f.agl_source.as_deref() == Some("baro") {
        AglSource::Baro
    } else {
        AglSource::Rangefinder
    };
    Some((m, src, true))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn link_loss_reads_the_spec_and_a_dash_for_an_unknown_action() {
        assert_eq!(link_loss_action_text(Some("rtl")), "Return home");
        assert_eq!(link_loss_action_text(Some("loiter")), "Hold");
        assert_eq!(link_loss_action_text(Some("warp")), "—");
        assert_eq!(link_loss_action_text(None), "—");
    }

    #[test]
    fn agl_marks_a_height_that_is_not_measured() {
        assert_eq!(AglSource::Rangefinder.text(), "AGL");
        assert_eq!(AglSource::Home.text(), "HOME");
        assert_eq!(AglSource::Baro.text(), "BARO");
        assert!(!AglSource::Rangefinder.marked());
        assert!(AglSource::Home.marked());
    }
}
