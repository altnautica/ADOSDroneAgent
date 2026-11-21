//! The sensing chip's words in the Pilot block: what the pilot sees obstacles
//! with and the speed that still stops inside the range it sees clear
//! (`Sensing: 3D lidar · ≤ 4.2 m/s`), the cockpit spec's text.

use crate::model::autonomy::Pilot;
use crate::model::spec_text::spec_text;

/// `Sensing: 3D lidar · ≤ 4.2 m/s`, or `None` while the pilot reports no
/// sensing (an older agent).
pub(super) fn sensing_text(p: &Pilot) -> Option<String> {
    let s = p.sensing.as_ref()?;
    let tier = spec_text(&format!("pilot.sensing.tier.{}", s.tier.as_str()), &[]);
    // At most one decimal, as every surface prints a speed limit.
    let v = (f64::from(s.speed_limit_mps) * 10.0).round() / 10.0;
    Some(spec_text(
        "pilot.sensing.chip",
        &[("tier", &tier), ("v", &format!("{v}"))],
    ))
}
