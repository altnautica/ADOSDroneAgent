//! Where the Vehicle tab's blocks go. The Pilot block (the flight-state chip,
//! failsafe, link loss and alerts) is placed first: on a terminal too short
//! for the full band it keeps its rows and the attitude indicator folds into
//! a Flight row; the Autonomy block shrinks to its "… N more" line; the trend
//! charts go before either. Under 100 columns the Sensors panel moves below
//! the battery packs, so the Flight and GPS rows keep their width. From 150
//! columns the alerts leave the Pilot block for a column of their own,
//! sharing the width beside the Pilot column with the battery packs.

use ratatui::layout::{Constraint, Layout, Rect};

/// Rows of the attitude, flight, GPS and sensors band.
pub(super) const TOP_ROWS: u16 = 12;
/// Rows of the trend charts.
pub(super) const CHART_ROWS: u16 = 9;
/// Rows the Autonomy block keeps at least: its border and "… N more".
pub(super) const AUTONOMY_MIN_ROWS: u16 = 3;
/// Rows the Autonomy block keeps while the Pilot block's must-show rows still
/// fit: its border, its first two rows and "… N more".
const AUTONOMY_KEEP_ROWS: u16 = 5;
/// Below this width the band drops to two columns.
pub(super) const WIDE_COLS: u16 = 100;
/// Rows of the Sensors panel when it sits below the packs.
const SENSOR_ROWS: u16 = 5;
/// From this width the alerts take a column of their own.
const ALERTS_APART_COLS: u16 = 150;

/// Whether the alerts draw in their own column rather than in the Pilot
/// block at this width.
pub(super) fn alerts_apart(width: u16) -> bool {
    width >= ALERTS_APART_COLS
}

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
    /// Every alert line when the alerts draw apart (borders included).
    pub alerts: u16,
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
    /// The alerts' own column, from [`ALERTS_APART_COLS`] wide.
    pub alerts: Option<Rect>,
}

/// The Pilot and Autonomy column's width.
fn right_width(w: u16) -> u16 {
    (w / 2).clamp(40, 52).min(w.saturating_sub(30))
}

/// The right column: the Pilot block first, the Autonomy block the rest. A
/// long Pilot block (many alert lines) folds its optional rows before the
/// Autonomy block drops below its first rows; its must-show rows never fold.
fn right_column(area: Rect, n: &Needs) -> (Rect, Rect) {
    let floor = area
        .height
        .saturating_sub(n.autonomy.min(AUTONOMY_MIN_ROWS));
    let keep = area
        .height
        .saturating_sub(n.autonomy.min(AUTONOMY_KEEP_ROWS));
    let pilot_h = n.pilot.min(keep.max(n.pilot_min)).min(floor);
    let r = Layout::vertical([Constraint::Length(pilot_h), Constraint::Min(0)]).split(area);
    (r[0], r[1])
}

/// Whether the full band leaves too few rows for the Pilot block's rows that
/// must show and the Autonomy block's last line.
pub(super) fn is_short(height: u16, pilot_min: u16) -> bool {
    height < TOP_ROWS + pilot_min + AUTONOMY_MIN_ROWS
}

/// A band below the top split into the left part, the Pilot column and,
/// from [`ALERTS_APART_COLS`], the alerts column, which halves the width left
/// of the Pilot column with the left part.
fn lower_cols(area: Rect) -> (Rect, Rect, Option<Rect>) {
    let right = right_width(area.width);
    if !alerts_apart(area.width) {
        let c = Layout::horizontal([Constraint::Min(0), Constraint::Length(right)]).split(area);
        return (c[0], c[1], None);
    }
    let alerts = area.width.saturating_sub(right) / 2;
    let c = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(right),
        Constraint::Length(alerts),
    ])
    .split(area);
    (c[0], c[1], Some(c[2]))
}

pub(super) fn plan(area: Rect, n: &Needs) -> Plan {
    let wide = area.width >= WIDE_COLS;
    if is_short(area.height, n.pilot_min) {
        // Short: two columns from the top; the attitude reads as a row.
        let (left, right, alerts) = lower_cols(area);
        let left = Layout::vertical([
            Constraint::Length(n.flight),
            Constraint::Length(n.gps),
            Constraint::Min(0),
        ])
        .split(left);
        let (pilot, autonomy) = right_column(right, n);
        return Plan {
            flight: left[0],
            gps: left[1],
            packs: left[2],
            pilot,
            autonomy,
            alerts,
            ..Plan::default()
        };
    }
    let below = (n.pilot + n.autonomy).max(n.alerts);
    let charts = n.charts && area.height >= TOP_ROWS + CHART_ROWS + below;
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
    let (left, right, alerts) = lower_cols(rows[2]);
    let (packs, sensors) = match sensors {
        Some(s) => (left, Some(s)),
        None if left.height >= SENSOR_ROWS + 3 => {
            let l =
                Layout::vertical([Constraint::Min(0), Constraint::Length(SENSOR_ROWS)]).split(left);
            (l[0], Some(l[1]))
        }
        None => (left, None),
    };
    let (pilot, autonomy) = right_column(right, n);
    Plan {
        attitude: Some(attitude),
        flight: mid[0],
        gps: mid[1],
        sensors,
        charts: charts.then_some(rows[1]),
        packs,
        pilot,
        autonomy,
        alerts,
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
            alerts: 0,
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

    /// At 160 columns the alerts get a column as wide as the packs beside
    /// the Pilot column, and the full height below the top band; under 150
    /// they stay in the Pilot block.
    #[test]
    fn a_wide_terminal_gives_the_alerts_a_column() {
        let p = plan(Rect::new(0, 0, 160, 46), &needs());
        let alerts = p.alerts.expect("alerts apart");
        assert_eq!(p.pilot.width, 52);
        assert!(
            alerts.width >= 54 && alerts.width >= p.packs.width,
            "{alerts:?} {:?}",
            p.packs
        );
        assert_eq!(alerts.y, p.pilot.y);
        assert_eq!(alerts.height, p.pilot.height + p.autonomy.height);
        assert!(plan(Rect::new(0, 0, 149, 46), &needs()).alerts.is_none());
        let short = plan(Rect::new(0, 0, 160, 18), &needs());
        assert!(short.attitude.is_none() && short.alerts.is_some());
    }
}
