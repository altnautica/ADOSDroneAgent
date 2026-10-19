//! The status OLED's rotating screens: `home` (the status frame) and the
//! ground-station service screens `gnss`, `radios` and `lora`.
//!
//! Rotation order is the configured `ground_station.ui.screens` order ∩
//! enabled (resolved by [`crate::panel_conf`]), filtered to the screens this
//! panel can paint: unknown ids and the TFT-only screens are skipped, and a
//! service screen is shown only while its service reports present. A 128x32
//! panel paints only `home`. When nothing renderable is left the panel shows
//! `home`, so it never goes dark. Each screen dwells [`DWELL`]; a single
//! renderable screen stays up statically.
//!
//! The service screens are five text lines of at most 20 columns in a mono
//! face, laid out top-down at a fixed pitch:
//!
//! ```text
//! GNSS  RTK-FIX  24sv   RADIOS 4              LORA IN duty 0.4%
//! base SURVEY 214s      1 RXTX -48 1.9% TX    TX OK  25mW
//! acc 1.4/2.0m          2 RX   -55 3.0%       heard 2  last 12s
//! PPS LOCK              3 RX   -61 4.2%       42 -92dB 7.5
//! hdg 274.3             4 MESH peers 2        7 -101dB 2.0
//! ```

use std::time::{Duration, Instant};

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::oled::OledGeometry;
use crate::pages::gs_ctx::{
    best_rssi, fix_label, loss_pct, pct_short, AdapterRole, GnssCtx, LoraCtx, RadioAdaptersCtx,
};
use crate::pages::PageContext;

/// How long each screen stays up before the next.
pub const DWELL: Duration = Duration::from_secs(5);

/// Text lines a service screen paints.
pub const SCREEN_LINES: usize = 5;

/// A screen the status OLED can paint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OledScreen {
    Home,
    Gnss,
    Radios,
    Lora,
}

impl OledScreen {
    /// The OLED screen for a configured screen id, `None` for an id this
    /// panel does not paint.
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "home" => Some(Self::Home),
            "gnss" => Some(Self::Gnss),
            "radios" => Some(Self::Radios),
            "lora" => Some(Self::Lora),
            _ => None,
        }
    }

    /// Whether the screen has something to show for `ctx`. A service that is
    /// absent or unreachable is skipped.
    fn available(self, ctx: &PageContext) -> bool {
        match self {
            Self::Home => true,
            Self::Gnss => ctx.gnss.is_present(),
            Self::Radios => ctx.radio_adapters.is_present(),
            Self::Lora => ctx.lora.is_present(),
        }
    }
}

/// The screens to rotate through, in configured order.
pub fn rotation(screens: &[String], ctx: &PageContext, geom: OledGeometry) -> Vec<OledScreen> {
    if geom.height < 64 {
        return vec![OledScreen::Home];
    }
    let mut out: Vec<OledScreen> = Vec::new();
    for s in screens.iter().filter_map(|id| OledScreen::from_id(id)) {
        if s.available(ctx) && !out.contains(&s) {
            out.push(s);
        }
    }
    if out.is_empty() {
        out.push(OledScreen::Home);
    }
    out
}

/// Steps through a rotation, one screen per [`DWELL`].
#[derive(Debug, Default)]
pub struct Rotator {
    index: usize,
    shown_at: Option<Instant>,
}

impl Rotator {
    /// The screen to show at `now` out of `screens` (never empty). Advances
    /// once the current screen has dwelt its time; a shrunk list wraps.
    pub fn current(&mut self, screens: &[OledScreen], now: Instant) -> OledScreen {
        let len = screens.len().max(1);
        match self.shown_at {
            None => self.shown_at = Some(now),
            Some(at) if now.duration_since(at) >= DWELL => {
                self.index += 1;
                self.shown_at = Some(now);
            }
            Some(_) => {}
        }
        self.index %= len;
        screens.get(self.index).copied().unwrap_or(OledScreen::Home)
    }
}

/// The mono face the service screens paint with (20 columns on 128 px).
pub fn screen_font() -> LoadedFont {
    LoadedFont::new(FontFace::MonoRegular, 10)
}

/// The five lines of a service screen; `None` for `home` or an absent
/// service.
pub fn screen_lines(screen: OledScreen, ctx: &PageContext) -> Option<Vec<String>> {
    match screen {
        OledScreen::Home => None,
        OledScreen::Gnss => ctx.gnss.present().map(gnss_lines),
        OledScreen::Radios => ctx
            .radio_adapters
            .present()
            .map(|r| radios_lines(r, ctx.mesh.peer_count)),
        OledScreen::Lora => ctx.lora.present().map(lora_lines),
    }
}

fn gnss_lines(g: &GnssCtx) -> Vec<String> {
    let sats = g.sats.map_or("--".to_string(), |s| s.to_string());
    let base = g.base.as_ref();
    let mode = base
        .and_then(|b| b.mode.as_deref())
        .unwrap_or("--")
        .to_ascii_uppercase();
    let base_line = match base.filter(|b| b.mode.as_deref() == Some("survey")) {
        Some(b) => format!("base {mode} {:.0}s", b.elapsed_s.unwrap_or(0.0)),
        None => format!("base {mode}"),
    };
    let acc = match base.map(|b| (b.accuracy_m, b.target_accuracy_m)) {
        Some((Some(a), Some(t))) => format!("acc {a:.1}/{t:.1}m"),
        Some((Some(a), None)) => format!("acc {a:.1}m"),
        _ => "acc --".to_string(),
    };
    let pps = match g.pps_locked {
        Some(true) => "PPS LOCK",
        Some(false) => "PPS NO LOCK",
        None => "PPS --",
    };
    let hdg = g
        .heading_deg
        .map_or("hdg --".to_string(), |h| format!("hdg {h:.1}"));
    vec![
        format!("GNSS  {}  {sats}sv", fix_label(g.fix.as_deref())),
        base_line,
        acc,
        pps.to_string(),
        hdg,
    ]
}

fn radios_lines(r: &RadioAdaptersCtx, mesh_peers: Option<i64>) -> Vec<String> {
    let mut lines = vec![format!("RADIOS {}", r.adapters.len())];
    for (i, a) in r.adapters.iter().take(SCREEN_LINES - 1).enumerate() {
        let role = a.role.as_str().to_ascii_uppercase();
        let line = if a.role == AdapterRole::Mesh {
            match mesh_peers {
                Some(p) => format!("{} MESH peers {p}", i + 1),
                None => format!("{} MESH", i + 1),
            }
        } else {
            let rssi = best_rssi(a).map_or("--".to_string(), |v| v.to_string());
            let loss = loss_pct(a).map_or("--".to_string(), |v| format!("{v:.1}%"));
            let tx = if a.tx_active { " TX" } else { "" };
            format!("{} {role:<4} {rssi} {loss}{tx}", i + 1)
        };
        lines.push(line);
    }
    lines
}

fn lora_lines(l: &LoraCtx) -> Vec<String> {
    let mut head = format!("LORA {}", l.region_label());
    if let Some(d) = l.top_duty() {
        head.push_str(&format!(" duty {}%", pct_short(d.used_pct)));
    }
    let mut tx = l.tx_state().short();
    if let Some(erp) = l.erp_mw {
        tx.push_str(&format!("  {erp:.0}mW"));
    }
    let heard = match l.beacons.first().and_then(|b| b.age_s) {
        Some(age) => format!("heard {}  last {age}s", l.beacons_heard),
        None => format!("heard {}", l.beacons_heard),
    };
    let mut lines = vec![head, tx, heard];
    for b in &l.beacons {
        let rssi = b.rssi_dbm.map_or("--".to_string(), |v| format!("{v:.0}dB"));
        let snr = b.snr_db.map_or("--".to_string(), |v| format!("{v:.1}"));
        lines.push(format!("{} {rssi} {snr}", b.node_id));
    }
    lines
}

/// Paint `lines` top-down onto an unpacked 1bpp canvas, [`SCREEN_LINES`]
/// lines spread evenly over the panel height.
pub fn render_lines(lines: &[String], geom: OledGeometry, font: &LoadedFont) -> Vec<u8> {
    let w = geom.width as usize;
    let h = geom.height as usize;
    let mut canvas = vec![0u8; w * h];
    let pitch = h as i32 / SCREEN_LINES as i32;
    for (i, line) in lines.iter().take(SCREEN_LINES).enumerate() {
        super::draw_line(&mut canvas, w, h, font, line, 0, i as i32 * pitch);
    }
    canvas
}

#[cfg(test)]
#[path = "screens_tests.rs"]
mod tests;
