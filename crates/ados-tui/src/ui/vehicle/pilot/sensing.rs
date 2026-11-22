//! What the pilot senses and flies, in the Pilot block: the sensing chip's
//! words, what it sees obstacles with and the speed that still stops inside
//! the range it sees clear (`Sensing: 3D lidar · ≤ 4.2 m/s`), the goal the
//! flying skill sent (`Follow intent · view 15° 8 m`, `Ring`), and the
//! moving obstacles it tracks (`Movers: 3 · nearest person 7.4 m`), the
//! cockpit spec's text.

use ados_protocol::pilot::{GoalKind, MoverKind};

use crate::model::autonomy::Pilot;
use crate::model::spec_text::spec_text;

/// The sensing chip, the goal chip and the movers line, each while the
/// pilot reports it.
pub(super) fn texts(p: &Pilot, local_pos: Option<(f64, f64)>) -> impl Iterator<Item = String> {
    [sensing_text(p), goal_text(p), movers_text(p, local_pos)]
        .into_iter()
        .flatten()
}

/// `Follow intent · view 15° 8 m` while the pilot reports the viewpoint it
/// picked for an intent, else the kind's label (`Ring`, `Orbit`, …); `None`
/// while no skill goal is reported.
fn goal_text(p: &Pilot) -> Option<String> {
    let kind = p.goal_kind?;
    let label = spec_text(&format!("pilot.goal_kind.{}", kind.as_str()), &[]);
    let Some(v) = p.view.filter(|_| kind == GoalKind::FollowIntent) else {
        return Some(label);
    };
    let bearing = (f64::from(v.bearing_deg).round() as i64).rem_euclid(360);
    let r = f64::from(v.radius_m).round();
    Some(spec_text(
        "pilot.goal_kind.view",
        &[
            ("kind", &label),
            ("bearing", &bearing.to_string()),
            ("r", &format!("{r}")),
        ],
    ))
}

/// `Sensing: 3D lidar · ≤ 4.2 m/s`, or `None` while the pilot reports no
/// sensing (an older agent).
fn sensing_text(p: &Pilot) -> Option<String> {
    let s = p.sensing.as_ref()?;
    let tier = spec_text(&format!("pilot.sensing.tier.{}", s.tier.as_str()), &[]);
    // At most one decimal, as every surface prints a speed limit.
    let v = (f64::from(s.speed_limit_mps) * 10.0).round() / 10.0;
    Some(spec_text(
        "pilot.sensing.chip",
        &[("tier", &tier), ("v", &format!("{v}"))],
    ))
}

/// `Movers: 3 · nearest person 7.4 m` (the horizontal distance from the FC's
/// local position; `Movers: 3` without a person or a position), or `None`
/// while the pilot tracks none.
fn movers_text(p: &Pilot, local_pos: Option<(f64, f64)>) -> Option<String> {
    if p.movers.is_empty() {
        return None;
    }
    let n = p.movers.len().to_string();
    let nearest = local_pos.and_then(|(north, east)| {
        p.movers
            .iter()
            .filter(|m| m.kind == MoverKind::Person)
            .map(|m| (m.ne[0] - north).hypot(m.ne[1] - east))
            .min_by(f64::total_cmp)
    });
    Some(match nearest {
        Some(d) => spec_text(
            "pilot.movers.count_person",
            &[("n", &n), ("d", &format!("{d:.1}"))],
        ),
        None => spec_text("pilot.movers.count", &[("n", &n)]),
    })
}
