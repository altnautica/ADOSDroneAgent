//! The attitude indicator: a horizon line rotated by roll and offset by
//! pitch, a ±30° pitch ladder, and a fixed aircraft marker, drawn in braille.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as Seg};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::app::Core;
use crate::model::vehicle::Telemetry;
use crate::ui::widgets::panel;

/// Canvas units per degree of pitch: ±30° fills the half-height.
const PITCH_SCALE: f64 = 0.9 / 30.0;

/// A world-frame point (horizontal `x`, pitch-scaled `y`) rotated into the
/// screen by the roll angle (banking right turns the world counter-clockwise).
fn rotate(x: f64, y: f64, roll: f64) -> (f64, f64) {
    let (s, c) = roll.sin_cos();
    (x * c - y * s, x * s + y * c)
}

/// The horizon and ladder segments for a roll/pitch in radians:
/// `(x1, y1, x2, y2, rung_degrees)` where rung 0 is the horizon.
pub fn segments(roll: f64, pitch: f64) -> Vec<(f64, f64, f64, f64, i32)> {
    let pitch_deg = pitch.to_degrees();
    let mut out = Vec::new();
    for rung in [-30, -20, -10, 0, 10, 20, 30] {
        // A rung `rung` degrees above the horizon sits at world y relative to
        // the aircraft's nose.
        let y = (f64::from(rung) - pitch_deg) * PITCH_SCALE;
        let half = if rung == 0 {
            3.0
        } else if rung % 20 == 0 {
            0.35
        } else {
            0.22
        };
        let (x1, y1) = rotate(-half, y, roll);
        let (x2, y2) = rotate(half, y, roll);
        out.push((x1, y1, x2, y2, rung));
    }
    out
}

pub fn render(
    f: &mut Frame,
    area: Rect,
    core: &Core,
    tel: Option<&Telemetry>,
    hidden: Option<&str>,
) {
    let t = &core.theme;
    let block = panel(t, core.sym, "Attitude");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let (roll, pitch) = match (hidden, tel.and_then(|x| x.roll.zip(x.pitch))) {
        (None, Some(rp)) => rp,
        (why, _) => {
            let why = why.unwrap_or("no attitude reported");
            f.render_widget(
                Paragraph::new(Span::styled(why.to_string(), t.dim())).wrap(Wrap { trim: true }),
                inner,
            );
            return;
        }
    };
    let rows = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).split(inner);
    let canvas_area = rows[0];
    let half_w = f64::from(canvas_area.width) / f64::from(canvas_area.height.max(1)) / 2.0;
    let horizon = t.accent();
    let ladder = t.muted();
    let craft = t.fg();
    let segs = segments(roll, pitch);
    let canvas = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([-half_w, half_w])
        .y_bounds([-1.0, 1.0])
        .paint(move |ctx| {
            for (x1, y1, x2, y2, rung) in &segs {
                let color: Color = if *rung == 0 { horizon } else { ladder };
                ctx.draw(&Seg::new(*x1, *y1, *x2, *y2, color));
            }
            ctx.layer();
            for (x1, y1, _, _, rung) in &segs {
                if *rung != 0 && rung % 20 == 0 && (-1.0..=1.0).contains(y1) {
                    ctx.print(
                        *x1 - 0.25,
                        *y1,
                        Span::styled(format!("{}", rung.abs()), Style::default().fg(ladder)),
                    );
                }
            }
            // The aircraft: two wings and a centre dot, fixed to the screen.
            ctx.draw(&Seg::new(-0.45, 0.0, -0.12, 0.0, craft));
            ctx.draw(&Seg::new(0.12, 0.0, 0.45, 0.0, craft));
            ctx.draw(&Seg::new(-0.12, 0.0, 0.0, -0.08, craft));
            ctx.draw(&Seg::new(0.0, -0.08, 0.12, 0.0, craft));
        });
    f.render_widget(canvas, canvas_area);
    let line = Line::from(vec![
        Span::styled("roll ", t.dim()),
        Span::styled(format!("{:+.1}°", roll.to_degrees()), t.bright()),
        Span::styled("  pitch ", t.dim()),
        Span::styled(format!("{:+.1}°", pitch.to_degrees()), t.bright()),
    ]);
    f.render_widget(Paragraph::new(line).centered(), rows[1]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_flight_draws_a_flat_centred_horizon() {
        let segs = segments(0.0, 0.0);
        let h = segs.iter().find(|s| s.4 == 0).unwrap();
        assert!(h.1.abs() < 1e-9 && h.3.abs() < 1e-9);
    }

    #[test]
    fn nose_up_moves_the_horizon_down_and_right_bank_raises_its_right_end() {
        let up = segments(0.0, 10f64.to_radians());
        let h = up.iter().find(|s| s.4 == 0).unwrap();
        assert!(h.1 < 0.0 && h.3 < 0.0);
        let bank = segments(20f64.to_radians(), 0.0);
        let h = bank.iter().find(|s| s.4 == 0).unwrap();
        assert!(h.3 > h.1, "right end higher than left");
    }
}
