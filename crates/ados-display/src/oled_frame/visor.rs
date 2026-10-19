//! The visor's status OLED: the body of its `home` status frame and the
//! `cameras`, `sensors` and `storage` screens, drawn from the sensing routes
//! the visor panel polls (`/api/v1/sensors`, `/api/v1/clock`,
//! `/api/v1/storage` and the capture state). A visor has no ground-station
//! link, so none of these lines describe one.
//!
//! ```text
//! CAMERAS 2 RECORDING   SENSORS 5             STORAGE OK
//! cam_fl   20.0/20fps   hub up  fc up         free 745/931G
//! cam_fr   19.8/20fps   gnss 3d 14sv          budget 15%
//!                       clock lock 0.42ms     pending 3
//!                       rates ok 4/4          write 1450MB/s
//! ```

use crate::graphics::thresholds::SENSOR_RATE_PCT;
use crate::pages::gs_ctx::fix_short;
use crate::pages::sensing_ctx::{fmt_num, SensingCtx, SensorInfo, StorageInfo, DASH};

use super::screens::SCREEN_LINES;

/// Bytes per GiB.
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Columns of the camera id on a camera row.
const CAMERA_ID_COLS: usize = 8;

/// The status frame body under the hostname: the LAN IP, the capture state,
/// the clock and, on a tall panel with storage reporting, the capture free
/// space.
pub fn status_lines(s: &SensingCtx, ip: &str, tall: bool) -> Vec<String> {
    let mut lines = vec![ip.to_string(), capture_line(s), clock_line(s)];
    if let Some(st) = s.storage.present().filter(|_| tall) {
        lines.push(free_line(st));
    }
    lines
}

/// `capture <state>`, `--` while the capture state is not read.
fn capture_line(s: &SensingCtx) -> String {
    let state = s.capture.present().and_then(|c| c.state.as_deref());
    format!("capture {}", state.unwrap_or(DASH))
}

/// `clock lock <sigma>ms` / `clock unlocked`, `--` while the clock is not read.
fn clock_line(s: &SensingCtx) -> String {
    match s.clock.present() {
        Some(k) if k.locked => {
            format!("clock lock {}ms", fmt_num(k.sigma_ns.map(|n| n / 1.0e6), 2))
        }
        Some(_) => "clock unlocked".to_string(),
        None => format!("clock {DASH}"),
    }
}

/// Free / total GiB of the capture volume; `--` with no capture volume.
fn free_line(st: &StorageInfo) -> String {
    match (st.free_bytes, st.total_bytes) {
        (Some(f), Some(t)) => format!("free {:.0}/{:.0}G", f / GIB, t / GIB),
        _ => format!("free {DASH}"),
    }
}

/// The `cameras` screen; `None` while no camera is known.
pub fn cameras_lines(s: &SensingCtx) -> Option<Vec<String>> {
    let capture = s.capture.present();
    let capture_fps = |id: &str| {
        capture
            .and_then(|c| c.per_camera.iter().find(|p| p.id == id))
            .and_then(|p| p.fps)
    };
    // Camera sensors from the sensors table; without one, the capture
    // state's camera list.
    let mut rows: Vec<String> = s
        .sensors
        .present()
        .map(|x| {
            x.sensors
                .iter()
                .filter(|c| c.kind == "camera")
                .map(|c| camera_row(&c.id, capture_fps(&c.id).or(c.rate_hz), c.expected_hz))
                .collect()
        })
        .unwrap_or_default();
    if rows.is_empty() {
        rows = capture
            .map(|c| {
                c.per_camera
                    .iter()
                    .map(|p| camera_row(&p.id, p.fps, None))
                    .collect()
            })
            .unwrap_or_default();
    }
    if rows.is_empty() {
        return None;
    }
    let mut head = format!("CAMERAS {}", rows.len());
    if let Some(state) = capture.and_then(|c| c.state.as_deref()) {
        head.push(' ');
        head.push_str(&state.to_ascii_uppercase());
    }
    let mut lines = vec![head];
    lines.extend(rows.into_iter().take(SCREEN_LINES - 1));
    Some(lines)
}

/// `<id> <fps>/<expected>fps`, the expected rate left out when unknown.
fn camera_row(id: &str, fps: Option<f64>, expected: Option<f64>) -> String {
    let id: String = id.chars().take(CAMERA_ID_COLS).collect();
    match expected {
        Some(e) => format!("{id:<8} {}/{e:.0}fps", fmt_num(fps, 1)),
        None => format!("{id:<8} {}fps", fmt_num(fps, 1)),
    }
}

/// The `sensors` screen; `None` while the sensors table is not read.
pub fn sensors_lines(s: &SensingCtx) -> Option<Vec<String>> {
    let x = s.sensors.present()?;
    let hub = match &x.hub {
        Some(h) if h.connected => "hub up",
        Some(_) => "hub down",
        None => "no hub",
    };
    let fc = match x.fc_connected {
        Some(true) => "fc up",
        Some(false) => "fc down",
        None => "no fc",
    };
    let gnss = match &x.gnss {
        Some(g) => format!(
            "gnss {} {}sv",
            fix_short(g.fix.as_deref()),
            g.sats.map_or_else(|| DASH.to_string(), |n| n.to_string())
        ),
        None => "no gnss".to_string(),
    };
    Some(vec![
        format!("SENSORS {}", x.sensors.len()),
        format!("{hub}  {fc}"),
        gnss,
        clock_line(s),
        rates_line(&x.sensors),
    ])
}

/// `rates ok <n>/<m>` over the sensors with an expected rate; `--` when none
/// has one.
fn rates_line(sensors: &[SensorInfo]) -> String {
    let graded: Vec<f64> = sensors.iter().filter_map(SensorInfo::rate_pct).collect();
    if graded.is_empty() {
        return format!("rates {DASH}");
    }
    let ok = graded
        .iter()
        .filter(|p| **p >= SENSOR_RATE_PCT.success_at)
        .count();
    format!("rates ok {ok}/{}", graded.len())
}

/// The `storage` screen; `None` while capture storage is not read.
pub fn storage_lines(s: &SensingCtx) -> Option<Vec<String>> {
    let st = s.storage.present()?;
    let state = st.state.as_deref().unwrap_or(DASH).to_ascii_uppercase();
    let pending = st
        .pending_upload()
        .map_or_else(|| DASH.to_string(), |n| n.to_string());
    Some(vec![
        format!("STORAGE {state}"),
        free_line(st),
        format!("budget {}%", fmt_num(st.budget_use_pct(), 0)),
        format!("pending {pending}"),
        format!("write {}MB/s", fmt_num(st.write_mbps, 0)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::reading::Reading;
    use crate::pages::sensing_ctx::test_util::full_ctx;

    fn sensing() -> SensingCtx {
        full_ctx(1000.0).sensing
    }

    #[test]
    fn the_status_body_shows_capture_clock_and_storage_never_a_link() {
        let lines = status_lines(&sensing(), "10.0.0.5", true);
        assert_eq!(
            lines,
            [
                "10.0.0.5",
                "capture recording",
                "clock lock 0.42ms",
                "free 745/931G"
            ]
        );
        assert!(lines.iter().all(|l| !l.contains("link")));
        // A short panel keeps the first three; nothing read reads `--`.
        let short = status_lines(&SensingCtx::default(), "no network", false);
        assert_eq!(short, ["no network", "capture --", "clock --"]);
    }

    #[test]
    fn camera_rows_use_the_capture_fps_against_the_expected_rate() {
        let lines = cameras_lines(&sensing()).unwrap();
        assert_eq!(lines, ["CAMERAS 1 RECORDING", "cam_fl   20.0/20fps"]);
        // Only the capture state: its cameras, with no expected rate.
        let mut s = sensing();
        s.sensors = Reading::Absent;
        let lines = cameras_lines(&s).unwrap();
        assert_eq!(lines[1], "cam_fl   20.0fps");
        assert_eq!(cameras_lines(&SensingCtx::default()), None);
    }

    #[test]
    fn sensors_screen_lists_links_gnss_clock_and_rates() {
        let lines = sensors_lines(&sensing()).unwrap();
        assert_eq!(
            lines,
            [
                "SENSORS 2",
                "hub up  fc up",
                "gnss 3d 14sv",
                "clock lock 0.42ms",
                "rates ok 2/2"
            ]
        );
        let slow = full_ctx(100.0).sensing;
        assert_eq!(sensors_lines(&slow).unwrap()[4], "rates ok 1/2");
        assert_eq!(sensors_lines(&SensingCtx::default()), None);
    }

    #[test]
    fn storage_screen_reads_the_capture_volume_only() {
        let mut s = sensing();
        if let Some(st) = s.storage.present_mut() {
            st.used_bytes = Some(1.2e11);
        }
        let lines = storage_lines(&s).unwrap();
        assert_eq!(
            lines,
            [
                "STORAGE OK",
                "free 745/931G",
                "budget 15%",
                "pending 3",
                "write 1450MB/s"
            ]
        );
        if let Some(st) = s.storage.present_mut() {
            st.free_bytes = None;
            st.total_bytes = None;
        }
        assert_eq!(storage_lines(&s).unwrap()[1], "free --");
        assert_eq!(storage_lines(&SensingCtx::default()), None);
    }

    #[test]
    fn every_line_fits_twenty_columns() {
        let s = sensing();
        let all = [
            cameras_lines(&s).unwrap(),
            sensors_lines(&s).unwrap(),
            storage_lines(&s).unwrap(),
            status_lines(&s, "192.168.100.100", true),
        ];
        for line in all.iter().flatten() {
            assert!(line.chars().count() <= 20, "{line}");
        }
    }
}
