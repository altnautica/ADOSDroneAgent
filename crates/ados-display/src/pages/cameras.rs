//! Cameras page: one two-line row per camera sensor.
//!
//! Line one: id / role and fps against the expected rate (rate verdict colour).
//! Line two: exposure, gain and lock state, last frame age and trigger mode.
//! Values come from `/api/v1/sensors` and the capture state; a missing value
//! reads `--`.

use crate::graphics::palette::Palette;
use crate::graphics::primitives::Canvas;
use crate::pages::sensing_ctx::{
    fmt_num, fmt_str, paint_absent, paint_frame, paint_status, paint_value, rate_color,
    CaptureCamera, ExposureInfo, SensorInfo, LABEL_X,
};
use crate::pages::{blank_panel, Chrome, HitZone, Page, PageContext};

/// Cameras that fit the content region at two rows each.
const MAX_CAMERAS: usize = 4;

/// The cameras surface, registered as `cameras`.
#[derive(Debug, Default, Clone, Copy)]
pub struct CamerasPage;

/// The exposure lock label: `requested` while a lock is not yet applied.
pub(crate) fn lock_label(e: Option<&ExposureInfo>) -> &'static str {
    match e {
        None => "--",
        Some(e) if e.locked && !e.applied => "requested",
        Some(e) if e.locked => "locked",
        Some(_) => "auto",
    }
}

/// Paint one camera at rows `2i` and `2i + 1`.
fn paint_camera(
    c: &mut Canvas,
    palette: &Palette,
    i: i32,
    s: &SensorInfo,
    cap: Option<&CaptureCamera>,
    exp: Option<&ExposureInfo>,
) {
    let fps = cap.and_then(|c| c.fps).or(s.rate_hz);
    let view = SensorInfo {
        rate_hz: fps,
        ..s.clone()
    };
    let name = match s.role.as_deref() {
        Some(r) if !r.is_empty() => format!("{} {r}", s.id),
        _ => s.id.clone(),
    };
    let rate = match s.expected_hz {
        Some(e) => format!("{} / {} fps", fmt_num(fps, 1), fmt_num(Some(e), 0)),
        None => format!("{} fps", fmt_num(fps, 1)),
    };
    let dropped = cap
        .and_then(|c| c.dropped)
        .map_or_else(String::new, |d| format!("  drop {d}"));
    let row = 2 * i;
    paint_status(
        c,
        row,
        LABEL_X,
        &format!("{name}  {rate}{dropped}"),
        rate_color(palette, &view),
    );
    let detail = format!(
        "exp {} us  g {}  {}  age {} ms  trig {}",
        fmt_num(exp.and_then(|e| e.exposure_us), 0),
        fmt_num(exp.and_then(|e| e.gain), 1),
        lock_label(exp),
        fmt_num(s.age_ms, 0),
        fmt_str(cap.and_then(|c| c.trigger.as_deref())),
    );
    paint_value(c, row + 1, LABEL_X + 14, &detail, palette.text_secondary);
}

impl Page for CamerasPage {
    fn id(&self) -> &'static str {
        "cameras"
    }

    fn chrome(&self) -> Chrome {
        Chrome::Tabbed
    }

    fn refresh_hz(&self) -> f32 {
        1.0
    }

    fn render(&self, ctx: &PageContext, palette: &Palette) -> Canvas {
        let mut canvas = blank_panel(palette);
        paint_frame(&mut canvas, ctx, palette, "CAMERAS");
        let capture = ctx.sensing.capture.as_ref();
        let exp = capture.and_then(|c| c.exposure.as_ref());
        // Camera sensors from the rig; without a sensors read fall back to the
        // capture state's camera list.
        let mut cams: Vec<SensorInfo> = ctx
            .sensing
            .sensors
            .as_ref()
            .map(|s| {
                s.sensors
                    .iter()
                    .filter(|x| x.kind == "camera")
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if cams.is_empty() {
            cams = capture
                .map(|c| {
                    c.per_camera
                        .iter()
                        .map(|p| SensorInfo {
                            id: p.id.clone(),
                            kind: "camera".into(),
                            ..SensorInfo::default()
                        })
                        .collect()
                })
                .unwrap_or_default();
        }
        if cams.is_empty() {
            paint_absent(&mut canvas, palette, "Cameras");
            return canvas;
        }
        for (i, s) in cams.iter().take(MAX_CAMERAS).enumerate() {
            let cap = capture.and_then(|c| c.per_camera.iter().find(|p| p.id == s.id));
            paint_camera(&mut canvas, palette, i as i32, s, cap, exp);
        }
        canvas
    }

    fn hit_zones(&self, _ctx: &PageContext) -> Vec<HitZone> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;
    use crate::pages::sensing_ctx::test_util::{
        assert_renders_and_leaves_tab_band, full_ctx, row_has_color,
    };

    #[test]
    fn renders_full_and_empty_without_touching_the_tab_band() {
        assert_eq!(CamerasPage.id(), "cameras");
        assert_renders_and_leaves_tab_band(&CamerasPage);
    }

    #[test]
    fn lock_reads_requested_until_applied() {
        let mut e = ExposureInfo {
            locked: true,
            ..ExposureInfo::default()
        };
        assert_eq!(lock_label(Some(&e)), "requested");
        e.applied = true;
        assert_eq!(lock_label(Some(&e)), "locked");
        e.locked = false;
        assert_eq!(lock_label(Some(&e)), "auto");
        assert_eq!(lock_label(None), "--");
    }

    #[test]
    fn slow_camera_reads_bad() {
        let mut ctx = full_ctx(1000.0);
        if let Some(c) = ctx.sensing.capture.as_mut() {
            c.per_camera[0].fps = Some(5.0);
        }
        let c = CamerasPage.render(&ctx, &DARK);
        assert!(row_has_color(&c, 0, LABEL_X, DARK.status_error));
    }
}
