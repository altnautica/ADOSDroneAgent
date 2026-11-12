//! Where the Vehicle tab's blocks go. The Pilot block (the flight-state chip,
//! failsafe, link loss and alerts) is placed first: on a terminal too short
//! for the full band it keeps its rows and the attitude indicator folds into
//! a Flight row; the Autonomy block shrinks to its "… N more" line; the trend
//! charts go before either. Under 100 columns the Sensors panel moves below
//! the battery packs, so the Flight and GPS rows keep their width.

use ratatui::layout::{Constraint, Layout, Rect};

/// Rows of the attitude, flight, GPS and sensors band.
pub(super) const TOP_ROWS: u16 = 12;
/// Rows of the trend charts.
pub(super) const CHART_ROWS: u16 = 9;
/// Rows the Autonomy block keeps at least: its border and "… N more".
pub(super) const AUTONOMY_MIN_ROWS: u16 = 3;
/// Below this width the band drops to two columns.
pub(super) const WIDE_COLS: u16 = 100;
/// Rows of the Sensors panel when it sits below the packs.
const SENSOR_ROWS: u16 = 5;

/// What each block asks for, in rows (borders included).
#[derive(Debug, Clone, Copy)]
pub(super) struct Needs {
    /// Every Pilot line.
    pub pilot: u16,
    /// The Pilot rows that must show, plus "… N more" when lines are cut.
    pub pilot_min: u16,
    pub autonomy: u16,
    pub flight: u16,
    pub gps: u16,
    /// The trend charts may show (only while the flight readings are live).
    pub charts: bool,
}

/// The blocks' rects. `attitude` is `None` when the attitude indicator
/// folds into the Flight panel.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Plan {
    pub attitude: Option<Rect>,
    pub flight: Rect,
    pub gps: Rect,
    pub sensors: Option<Rect>,
    pub charts: Option<Rect>,
    pub packs: Rect,
    pub pilot: Rect,
    pub autonomy: Rect,
}

/// The Pilot and Autonomy column's width.
fn right_width(w: u16) -> u16 {
    (w / 2).clamp(40, 52).min(w.saturating_sub(30))
}

/// The right column: the Pilot block first, the Autonomy block the rest.
fn right_column(area: Rect, n: &Needs) -> (Rect, Rect) {
    let pilot_h = n.pilot.min(
        area.height
            .saturating_sub(n.autonomy.min(AUTONOMY_MIN_ROWS)),
    );
    let r = Layout::vertical([Constraint::Length(pilot_h), Constraint::Min(0)]).split(area);
    (r[0], r[1])
}

/// Whether the full band leaves too few rows for the Pilot block's rows that
/// must show and the Autonomy block's last line.
pub(super) fn is_short(height: u16, pilot_min: u16) -> bool {
    height < TOP_ROWS + pilot_min + AUTONOMY_MIN_ROWS
}

pub(super) fn plan(area: Rect, n: &Needs) -> Plan {
    let wide = area.width >= WIDE_COLS;
    let cols = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(right_width(area.width)),
    ]);
    if is_short(area.height, n.pilot_min) {
        // Short: two columns from the top; the attitude reads as a row.
        let c = cols.split(area);
        let left = Layout::vertical([
            Constraint::Length(n.flight),
            Constraint::Length(n.gps),
            Constraint::Min(0),
        ])
        .split(c[0]);
        let (pilot, autonomy) = right_column(c[1], n);
        return Plan {
            flight: left[0],
            gps: left[1],
            packs: left[2],
            pilot,
            autonomy,
            ..Plan::default()
        };
    }
    let charts = n.charts && area.height >= TOP_ROWS + CHART_ROWS + n.pilot + n.autonomy;
    let rows = Layout::vertical([
        Constraint::Length(TOP_ROWS),
        Constraint::Length(if charts { CHART_ROWS } else { 0 }),
        Constraint::Min(0),
    ])
    .split(area);
    let (attitude, mid, sensors) = if wide {
        let top = Layout::horizontal([
            Constraint::Length(36),
            Constraint::Min(30),
            Constraint::Length(40),
        ])
        .split(rows[0]);
        (top[0], top[1], Some(top[2]))
    } else {
        let top = Layout::horizontal([Constraint::Length(28), Constraint::Min(0)]).split(rows[0]);
        (top[0], top[1], None)
    };
    let mid = Layout::vertical([Constraint::Length(7), Constraint::Min(0)]).split(mid);
    let lower = cols.split(rows[2]);
    let (packs, sensors) = match sensors {
        Some(s) => (lower[0], Some(s)),
        None if lower[0].height >= SENSOR_ROWS + 3 => {
            let l = Layout::vertical([Constraint::Min(0), Constraint::Length(SENSOR_ROWS)])
                .split(lower[0]);
            (l[0], Some(l[1]))
        }
        None => (lower[0], None),
    };
    let (pilot, autonomy) = right_column(lower[1], n);
    Plan {
        attitude: Some(attitude),
        flight: mid[0],
        gps: mid[1],
        sensors,
        charts: charts.then_some(rows[1]),
        packs,
        pilot,
        autonomy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn needs() -> Needs {
        Needs {
            pilot: 17,
            pilot_min: 8,
            autonomy: 7,
            flight: 8,
            gps: 5,
            charts: true,
        }
    }

    #[test]
    fn a_short_terminal_keeps_the_pilot_rows_first() {
        let p = plan(Rect::new(0, 0, 78, 18), &needs());
        assert!(p.attitude.is_none(), "the attitude folds into a row");
        assert!(p.pilot.height >= 8, "{:?}", p.pilot);
        assert!(p.autonomy.height >= AUTONOMY_MIN_ROWS, "{:?}", p.autonomy);
        assert!(p.flight.width >= 38, "{:?}", p.flight);
    }

    #[test]
    fn a_narrow_tall_terminal_moves_sensors_below_the_packs() {
        let p = plan(Rect::new(0, 0, 90, 40), &needs());
        let (sensors, packs) = (p.sensors.expect("sensors"), p.packs);
        assert_eq!(sensors.x, packs.x);
        assert!(sensors.y > packs.y);
        assert!(p.flight.width >= 50, "{:?}", p.flight);
    }

    #[test]
    fn stale_readings_draw_no_charts() {
        let tall = Rect::new(0, 0, 160, 60);
        assert!(plan(tall, &needs()).charts.is_some());
        let stale = Needs {
            charts: false,
            ..needs()
        };
        assert!(plan(tall, &stale).charts.is_none());
    }
}
