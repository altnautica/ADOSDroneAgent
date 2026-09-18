//! Named formations.
//!
//! A formation is a table of per-slot offsets in a local NED frame anchored on
//! the fleet centroid or on a designated slot. The five built-ins are GENERATED
//! from the registered slot set rather than stored, so any fleet size from 1 to
//! 24 is valid and re-pairing a drone never leaves a hole in a table.
//!
//! The five names are a CLOSED set — they are the exact enum the Mission Control
//! settings page offers as a `<Select>`, so a sixth name here would be a value
//! the operator can never choose and a config value the UI cannot render.

use std::collections::BTreeMap;

use crate::geo::Ned;
use crate::neighbor::NeighborState;

/// Proportional gain on the formation position error, 1/s.
pub const FORMATION_GAIN: f64 = 1.0;

/// Default spacing between formation stations, metres (`swarm.default_spacing`).
pub const DEFAULT_SPACING_M: f64 = 10.0;

/// How many of the formation's own diameters out a fleet member may be and
/// still be averaged into the centroid anchor.
///
/// Four is generous: a member four formation-widths from us is still forming
/// up, and pulling the anchor toward it is the whole point of the law. What the
/// gate is for is the fix that is not a MEASUREMENT at all — see
/// [`anchor_position`].
pub const ANCHOR_RANGE_SLACK: f64 = 4.0;

/// Floor under the centroid anchor's range gate, metres.
///
/// A tight formation has a small diameter, so the slack alone would gate a
/// legitimately out-of-station member out of the anchor and stall the fleet
/// short of its shape. This floor keeps the gate wide enough to be invisible to
/// every real geometry — it is six times the flocking radius — while still
/// four orders of magnitude below the distance a bogus fix lands at.
pub const ANCHOR_RANGE_MIN_M: f64 = 200.0;

/// The five built-in formation shapes. Closed by design; see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FormationName {
    /// Abreast, along east.
    #[default]
    Line,
    /// Single file, along north, lowest slot leading.
    Column,
    /// A V with the lowest slot at the apex.
    Wedge,
    /// A row-major block, `ceil(sqrt(n))` wide.
    Grid,
    /// Evenly spaced on a ring sized so adjacent stations are one spacing apart.
    Circle,
}

impl FormationName {
    pub const ALL: [Self; 5] = [
        Self::Line,
        Self::Column,
        Self::Wedge,
        Self::Grid,
        Self::Circle,
    ];

    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Line => "line",
            Self::Column => "column",
            Self::Wedge => "wedge",
            Self::Grid => "grid",
            Self::Circle => "circle",
        }
    }

    /// Parse the config value. An unrecognised name falls back to `line` — the
    /// same default the config model carries, so a hand-edited typo flies the
    /// documented default instead of refusing to form up at all.
    pub fn from_wire(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "column" => Self::Column,
            "wedge" => Self::Wedge,
            "grid" => Self::Grid,
            "circle" => Self::Circle,
            _ => Self::Line,
        }
    }
}

/// What the offset table is measured from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FormationAnchor {
    /// The centroid of every fleet member this drone can see, including itself.
    #[default]
    Centroid,
    /// A designated slot. Absent from the neighbour table means no anchor, and
    /// the formation layer declines to command rather than guessing.
    Slot(u8),
}

/// A named formation: shape, anchor, and the per-slot station offsets in metres
/// north / east / down.
#[derive(Debug, Clone, PartialEq)]
pub struct Formation {
    pub name: String,
    pub anchor: FormationAnchor,
    pub offsets: BTreeMap<u8, [f32; 3]>,
}

impl Formation {
    /// Generate a built-in shape over `slots` at `spacing_m`.
    ///
    /// Every shape is translated so its centroid is the origin, which is what
    /// makes [`FormationAnchor::Centroid`] a fixed point: the fleet centroid is
    /// stationary under the formation law, so the swarm converges onto the shape
    /// instead of drifting while it forms.
    pub fn built_in(
        name: FormationName,
        slots: &[u8],
        spacing_m: f64,
        anchor: FormationAnchor,
    ) -> Self {
        let spacing = if spacing_m.is_finite() && spacing_m > 0.0 {
            spacing_m
        } else {
            DEFAULT_SPACING_M
        };
        let mut ordered: Vec<u8> = slots.to_vec();
        ordered.sort_unstable();
        ordered.dedup();

        let n = ordered.len();
        let mut raw: Vec<(f64, f64)> = Vec::with_capacity(n);
        let mid = if n > 1 { (n - 1) as f64 / 2.0 } else { 0.0 };
        for i in 0..n {
            let f = i as f64;
            raw.push(match name {
                FormationName::Line => (0.0, (f - mid) * spacing),
                FormationName::Column => ((mid - f) * spacing, 0.0),
                FormationName::Wedge => {
                    if i == 0 {
                        (0.0, 0.0)
                    } else {
                        let rank = i.div_ceil(2) as f64;
                        let side = if i.is_multiple_of(2) { -1.0 } else { 1.0 };
                        (-rank * spacing, side * rank * spacing)
                    }
                }
                FormationName::Grid => {
                    let cols = grid_columns(n);
                    let row = (i / cols) as f64;
                    let col = (i % cols) as f64;
                    (-row * spacing, col * spacing)
                }
                FormationName::Circle => {
                    if n < 2 {
                        (0.0, 0.0)
                    } else {
                        // Radius chosen so the chord between adjacent stations is
                        // exactly `spacing`, for every n. A naive
                        // `spacing * n / 2pi` collapses to 0.64 spacing at n = 2.
                        let r = spacing / (2.0 * (std::f64::consts::PI / n as f64).sin());
                        let theta = std::f64::consts::TAU * f / n as f64;
                        (r * theta.cos(), r * theta.sin())
                    }
                }
            });
        }

        let (mut cn, mut ce) = (0.0, 0.0);
        for (a, b) in &raw {
            cn += a;
            ce += b;
        }
        if n > 0 {
            cn /= n as f64;
            ce /= n as f64;
        }

        Self {
            name: name.as_wire().to_string(),
            anchor,
            offsets: ordered
                .into_iter()
                .zip(raw)
                .map(|(slot, (a, b))| (slot, [(a - cn) as f32, (b - ce) as f32, 0.0]))
                .collect(),
        }
    }

    /// This drone's station offset, if the table covers its slot.
    pub fn station(&self, slot: u8) -> Option<Ned> {
        self.offsets
            .get(&slot)
            .map(|o| Ned::new(o[0] as f64, o[1] as f64, o[2] as f64))
    }

    /// How far from this drone a fleet member may be and still resolve the
    /// anchor, metres.
    ///
    /// Derived from the table rather than configured, so it scales with spacing
    /// and fleet size automatically and there is no second number to keep in
    /// step with the shape. Two stations are at most one diameter apart, so
    /// `SLACK` diameters covers a fleet still converging onto the shape.
    pub fn anchor_range_m(&self) -> f64 {
        let mut max_sq = 0.0f64;
        for o in self.offsets.values() {
            let r_sq = (o[0] as f64).powi(2) + (o[1] as f64).powi(2) + (o[2] as f64).powi(2);
            if r_sq.is_finite() && r_sq > max_sq {
                max_sq = r_sq;
            }
        }
        (2.0 * max_sq.sqrt() * ANCHOR_RANGE_SLACK).max(ANCHOR_RANGE_MIN_M)
    }
}

/// Grid width: `ceil(sqrt(n))`, at least 1.
fn grid_columns(n: usize) -> usize {
    if n <= 1 {
        return 1;
    }
    let mut c = (n as f64).sqrt().ceil() as usize;
    while c * c < n {
        c += 1;
    }
    c.max(1)
}

/// The anchor's position in the local NED frame (this drone at the origin).
///
/// `Centroid` averages this drone and every fleet member whose fix is inside
/// `radius_m` ([`Formation::anchor_range_m`]). `Slot(s)` resolves to the origin
/// when `s` is this drone, else to that neighbour's position, and to `None`
/// when the designated slot is not being heard OR its fix is outside the gate —
/// a formation with a missing anchor must decline, not fall back to some other
/// reference and fly the fleet somewhere nobody commanded.
///
/// # Why the centroid needs a range gate at all
///
/// This is the only law in the crate that weights EVERY neighbour: the flocking
/// terms, the repulsive term and the closing-rate barrier all build a
/// [`crate::neighbor::NearestSet`] first, so an implausible fix falls out of
/// each of them on range. Ungated, one member beaconing the no-fix placeholder
/// position — a real geodetic point thousands of kilometres from any operating
/// site — moves the anchor by `distance / (N + 1)` metres, which saturates
/// [`crate::setpoint::MAX_COMMAND_SPEED_MPS`] for every drone in the fleet at
/// once. The closing-rate barrier cannot catch it either: it caps closure on
/// in-range neighbours and passes a command aimed at empty space straight
/// through. So the transport-side skip in [`crate::swarmbus::fixes_from_payload`]
/// is the first line and this gate is the backstop for any other producer.
pub fn anchor_position(
    anchor: FormationAnchor,
    own_slot: u8,
    neighbors: &[NeighborState],
    radius_m: f64,
) -> Option<Ned> {
    match anchor {
        FormationAnchor::Centroid => {
            let mut sum = Ned::ZERO;
            let mut used = 0usize;
            for n in neighbors {
                if n.is_within(radius_m) {
                    sum = sum + n.pos;
                    used += 1;
                }
            }
            // Divide by what was actually summed, never by the table length: a
            // gated-out member left in the divisor would shrink the anchor
            // toward this drone and bias the whole shape inward.
            Some(sum.scale(1.0 / (used + 1) as f64))
        }
        FormationAnchor::Slot(s) if s == own_slot => Some(Ned::ZERO),
        FormationAnchor::Slot(s) => neighbors
            .iter()
            .find(|n| n.slot == s && n.is_within(radius_m))
            .map(|n| n.pos),
    }
}

/// The formation-keeping velocity command, m/s NED, or `None` when this drone
/// has no station or the anchor cannot be resolved.
pub fn command(
    formation: &Formation,
    own_slot: u8,
    neighbors: &[NeighborState],
    gain: f64,
    cruise: f64,
) -> Option<Ned> {
    let station = formation.station(own_slot)?;
    let anchor = anchor_position(
        formation.anchor,
        own_slot,
        neighbors,
        formation.anchor_range_m(),
    )?;
    // Own position is the frame origin, so the error IS anchor + station.
    Some((anchor + station).scale(gain).clamp_norm(cruise))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::separation::min_pairwise_distance;

    const SPACING: f64 = 10.0;
    const SIZES: [usize; 4] = [1, 2, 7, 24];

    fn slots(n: usize) -> Vec<u8> {
        (1..=n as u8).collect()
    }

    fn stations(f: &Formation) -> Vec<Ned> {
        f.offsets
            .values()
            .map(|o| Ned::new(o[0] as f64, o[1] as f64, o[2] as f64))
            .collect()
    }

    #[test]
    fn every_built_in_is_valid_for_every_fleet_size() {
        for name in FormationName::ALL {
            for n in SIZES {
                let s = slots(n);
                let f = Formation::built_in(name, &s, SPACING, FormationAnchor::Centroid);
                assert_eq!(f.name, name.as_wire());
                // Covers exactly the registered slot set — no hole, no extra.
                assert_eq!(f.offsets.len(), n, "{name:?} n={n}");
                for slot in &s {
                    let st = f
                        .station(*slot)
                        .unwrap_or_else(|| panic!("{name:?} n={n} slot {slot}"));
                    assert!(st.is_finite(), "{name:?} n={n} slot {slot} -> {st:?}");
                }
                let ps = stations(&f);
                // Centroid at the origin, so the Centroid anchor is a fixed point.
                let mut c = Ned::ZERO;
                for p in &ps {
                    c = c + *p;
                }
                assert!(
                    c.scale(1.0 / n as f64).norm() < 1e-6,
                    "{name:?} n={n} centroid {:?}",
                    c.scale(1.0 / n as f64)
                );
                // No two stations closer than the commanded spacing, so a formed
                // fleet is never inside the separation layer's soft radius.
                if let Some(min) = min_pairwise_distance(&ps) {
                    assert!(
                        min >= SPACING * 0.999,
                        "{name:?} n={n} packs stations {min} m apart"
                    );
                }
            }
        }
    }

    #[test]
    fn shapes_are_the_shapes_their_names_claim() {
        let s = slots(4);
        let line = Formation::built_in(FormationName::Line, &s, SPACING, FormationAnchor::Centroid);
        for p in stations(&line) {
            assert!(p.n.abs() < 1e-9, "line is abreast, not staggered: {p:?}");
        }
        let column = Formation::built_in(
            FormationName::Column,
            &s,
            SPACING,
            FormationAnchor::Centroid,
        );
        for p in stations(&column) {
            assert!(p.e.abs() < 1e-9, "column is single file: {p:?}");
        }
        // The lowest slot leads a column.
        assert!(column.station(1).unwrap().n > column.station(4).unwrap().n);

        let wedge =
            Formation::built_in(FormationName::Wedge, &s, SPACING, FormationAnchor::Centroid);
        // Apex is the lowest slot and is the furthest forward.
        let apex = wedge.station(1).unwrap();
        for slot in 2..=4u8 {
            assert!(
                wedge.station(slot).unwrap().n < apex.n,
                "slot {slot} ahead of the apex"
            );
        }
        // Arms straddle the centreline.
        assert!(wedge.station(2).unwrap().e * wedge.station(3).unwrap().e < 0.0);

        let circle = Formation::built_in(
            FormationName::Circle,
            &s,
            SPACING,
            FormationAnchor::Centroid,
        );
        let radii: Vec<f64> = stations(&circle).iter().map(|p| p.norm()).collect();
        for r in &radii {
            assert!(
                (r - radii[0]).abs() < 1e-9,
                "circle radii differ: {radii:?}"
            );
        }
    }

    #[test]
    fn circle_spacing_holds_at_the_small_sizes_a_naive_radius_breaks() {
        // spacing * n / 2pi gives 0.318 * spacing radius at n = 2, i.e. adjacent
        // stations 0.64 spacing apart. The chord-derived radius gives exactly
        // spacing at every n.
        for n in [2usize, 3, 7, 24] {
            let f = Formation::built_in(
                FormationName::Circle,
                &slots(n),
                SPACING,
                FormationAnchor::Centroid,
            );
            let ps = stations(&f);
            let min = min_pairwise_distance(&ps).expect("n >= 2");
            // The table stores f32 metres, so at a 38 m radius (n = 24) the
            // representable resolution is a few micrometres. A millimetre is
            // tighter than any real formation cares about and still catches a
            // wrong radius formula by three orders of magnitude.
            assert!((min - SPACING).abs() < 1e-3, "n={n} adjacent chord {min}");
        }
    }

    #[test]
    fn grid_is_ceil_sqrt_wide() {
        assert_eq!(grid_columns(1), 1);
        assert_eq!(grid_columns(2), 2);
        assert_eq!(grid_columns(4), 2);
        assert_eq!(grid_columns(7), 3);
        assert_eq!(grid_columns(24), 5);
        let f = Formation::built_in(
            FormationName::Grid,
            &slots(7),
            SPACING,
            FormationAnchor::Centroid,
        );
        let easts: std::collections::BTreeSet<i64> = stations(&f)
            .iter()
            .map(|p| (p.e / SPACING).round() as i64)
            .collect();
        assert_eq!(easts.len(), 3, "three columns for n=7");
    }

    #[test]
    fn a_sparse_slot_set_is_honoured_verbatim() {
        // Slots 3, 11 and 19 — what a fleet looks like after two drones are
        // released. A generator keyed on slot number rather than rank would put
        // station 19 nineteen spacings out.
        let f = Formation::built_in(
            FormationName::Line,
            &[19, 3, 11],
            SPACING,
            FormationAnchor::Centroid,
        );
        assert_eq!(
            f.offsets.keys().copied().collect::<Vec<_>>(),
            vec![3, 11, 19]
        );
        let ps = stations(&f);
        assert!((min_pairwise_distance(&ps).unwrap() - SPACING).abs() < 1e-9);
        assert_eq!(f.station(7), None, "unregistered slot has no station");
    }

    #[test]
    fn duplicate_slots_collapse_and_bad_spacing_falls_back() {
        let f = Formation::built_in(
            FormationName::Grid,
            &[4, 4, 2, 2],
            f64::NAN,
            FormationAnchor::Centroid,
        );
        assert_eq!(f.offsets.len(), 2);
        let min = min_pairwise_distance(&stations(&f)).unwrap();
        assert!((min - DEFAULT_SPACING_M).abs() < 1e-9, "{min}");
        assert_eq!(
            Formation::built_in(
                FormationName::Line,
                &[1, 2],
                -5.0,
                FormationAnchor::Centroid
            )
            .station(1)
            .unwrap()
            .e,
            -DEFAULT_SPACING_M / 2.0
        );
    }

    #[test]
    fn wire_names_are_exactly_the_five_the_ui_offers() {
        let wire: Vec<&str> = FormationName::ALL.iter().map(|n| n.as_wire()).collect();
        assert_eq!(wire, vec!["line", "column", "wedge", "grid", "circle"]);
        for n in FormationName::ALL {
            assert_eq!(FormationName::from_wire(n.as_wire()), n);
            assert_eq!(FormationName::from_wire(&n.as_wire().to_uppercase()), n);
        }
        assert_eq!(FormationName::from_wire("  wedge "), FormationName::Wedge);
        assert_eq!(FormationName::from_wire("diamond"), FormationName::Line);
        assert_eq!(FormationName::from_wire(""), FormationName::Line);
    }

    #[test]
    fn centroid_anchor_averages_the_whole_fleet_including_self() {
        let ns = [
            NeighborState::new(2, Ned::new(30.0, 0.0, 0.0), Ned::ZERO, 0),
            NeighborState::new(3, Ned::new(0.0, 30.0, 0.0), Ned::ZERO, 0),
        ];
        let c = anchor_position(FormationAnchor::Centroid, 1, &ns, ANCHOR_RANGE_MIN_M)
            .expect("always resolvable");
        assert!(
            (c.n - 10.0).abs() < 1e-12 && (c.e - 10.0).abs() < 1e-12,
            "{c:?}"
        );
        // Alone: the centroid is this drone.
        assert_eq!(
            anchor_position(FormationAnchor::Centroid, 1, &[], ANCHOR_RANGE_MIN_M),
            Some(Ned::ZERO)
        );
    }

    #[test]
    fn a_fix_outside_the_gate_leaves_the_anchor_exactly_where_it_was() {
        // The no-fix placeholder position, as the local frame sees it: a real
        // geodetic point thousands of kilometres away. Averaged in, it would put
        // the anchor half a continent north and saturate the command.
        let bogus = NeighborState::new(3, Ned::new(8.5e6, -1.4e6, 0.0), Ned::ZERO, 0);
        let real = NeighborState::new(2, Ned::new(30.0, 0.0, 0.0), Ned::ZERO, 0);

        let without = anchor_position(FormationAnchor::Centroid, 1, &[real], ANCHOR_RANGE_MIN_M);
        let with = anchor_position(
            FormationAnchor::Centroid,
            1,
            &[real, bogus],
            ANCHOR_RANGE_MIN_M,
        );
        assert_eq!(with, without, "a gated fix must not enter the average");
        // Including the divisor: counting it would bias the anchor inward.
        assert!((with.expect("resolves").n - 15.0).abs() < 1e-12);

        // And a designated anchor slot that is out of range declines outright
        // rather than anchoring the fleet on a position nobody is at.
        assert_eq!(
            anchor_position(FormationAnchor::Slot(3), 1, &[bogus], ANCHOR_RANGE_MIN_M),
            None
        );
    }

    #[test]
    fn the_anchor_gate_is_wide_enough_to_be_invisible_to_a_real_formation() {
        // A full fleet at the default spacing. The gate is measured from THIS
        // drone, so the case it has to clear is the two stations furthest apart:
        // a drone on one of them must still see the other, or the law would
        // refuse to form up the shape it just generated.
        for name in FormationName::ALL {
            let f = Formation::built_in(name, &slots(24), SPACING, FormationAnchor::Centroid);
            let gate = f.anchor_range_m();
            let ps = stations(&f);
            let worst = ps
                .iter()
                .flat_map(|a| ps.iter().map(move |b| (*a - *b).norm()))
                .fold(0.0f64, f64::max);
            assert!(
                worst < gate,
                "{name:?} spans {worst} m against a {gate} m gate"
            );
        }
    }

    #[test]
    fn slot_anchor_declines_when_the_designated_slot_is_not_heard() {
        let ns = [NeighborState::new(2, Ned::new(5.0, 0.0, 0.0), Ned::ZERO, 0)];
        assert_eq!(
            anchor_position(FormationAnchor::Slot(1), 1, &ns, ANCHOR_RANGE_MIN_M),
            Some(Ned::ZERO)
        );
        assert_eq!(
            anchor_position(FormationAnchor::Slot(2), 1, &ns, ANCHOR_RANGE_MIN_M),
            Some(Ned::new(5.0, 0.0, 0.0))
        );
        assert_eq!(
            anchor_position(FormationAnchor::Slot(9), 1, &ns, ANCHOR_RANGE_MIN_M),
            None
        );
        let f = Formation::built_in(
            FormationName::Line,
            &[1, 2],
            SPACING,
            FormationAnchor::Slot(9),
        );
        assert_eq!(command(&f, 1, &ns, FORMATION_GAIN, 10.0), None);
    }

    #[test]
    fn command_drives_the_station_error_to_zero() {
        let f = Formation::built_in(
            FormationName::Line,
            &[1, 2],
            SPACING,
            FormationAnchor::Centroid,
        );
        // Slot 1's station is 5 m west of the centroid; slot 2 is 5 m east.
        // Neighbour 2 sits 10 m east of us, so the centroid is 5 m east and our
        // station is exactly where we already are: zero command.
        let ns = [NeighborState::new(
            2,
            Ned::new(0.0, 10.0, 0.0),
            Ned::ZERO,
            0,
        )];
        let c = command(&f, 1, &ns, FORMATION_GAIN, 10.0).expect("station and anchor resolve");
        assert!(c.norm() < 1e-9, "already on station: {c:?}");

        // Displace: neighbour 30 m east means the centroid is 15 m east and our
        // station is 10 m east of us.
        let ns = [NeighborState::new(
            2,
            Ned::new(0.0, 30.0, 0.0),
            Ned::ZERO,
            0,
        )];
        let c = command(&f, 1, &ns, FORMATION_GAIN, 100.0).expect("resolves");
        assert!((c.e - 10.0).abs() < 1e-9, "{c:?}");
        // And it saturates at cruise rather than commanding 10 m/s at an SBC.
        let capped = command(&f, 1, &ns, FORMATION_GAIN, 2.0).expect("resolves");
        assert!((capped.norm() - 2.0).abs() < 1e-12);
    }

    #[test]
    fn an_unregistered_drone_gets_no_formation_command() {
        let f = Formation::built_in(
            FormationName::Wedge,
            &[1, 2, 3],
            SPACING,
            FormationAnchor::Centroid,
        );
        assert_eq!(command(&f, 9, &[], FORMATION_GAIN, 10.0), None);
    }
}
