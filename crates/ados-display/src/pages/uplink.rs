//! Uplink detail page.
//!
//! The drilldown from the dashboard's uplink/cloud tile. It shows the
//! cloud-relay state (a status badge plus MQTT/HTTP/RTT and the drone id or
//! pair code) across the top band, and a cellular band (signal bars,
//! RSRP/RSRQ/SINR, band, IP, tech) below — or a WiFi-client fallback when no
//! modem is present, or a "No WAN uplink" notice when neither is up.
//!
//! Cloud state comes from [`PageContext::cloud`], the modem from
//! [`PageContext::uplink`], and the WiFi fallback from [`PageContext::network`].

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_circle, fill_rect_outline, text, Canvas};
use crate::pages::{
    blank_panel, CloudCtx, HitAction, HitZone, NetworkCtx, Page, PageContext, UplinkCtx,
};
use crate::widgets::{draw_detail_header, DETAIL_HEADER_H};

/// Header band height shared by every detail modal.
const HEADER_H: i32 = DETAIL_HEADER_H;

const CLOUD_BAND_Y: i32 = HEADER_H + 4;
const CLOUD_BAND_H: i32 = 86;
const CELL_BAND_Y: i32 = CLOUD_BAND_Y + CLOUD_BAND_H + 4;

/// The badge severity that maps a cloud state to a color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    Ok,
    Warn,
    Err,
    Muted,
}

/// Map a measured RSRP to a 0..=4 signal-bar count.
fn bars_for_rsrp(rsrp_dbm: Option<f64>) -> i32 {
    match rsrp_dbm {
        None => 0,
        Some(v) if v >= -90.0 => 4,
        Some(v) if v >= -100.0 => 3,
        Some(v) if v >= -110.0 => 2,
        Some(_) => 1,
    }
}

/// Return the cloud badge label + severity from the cloud state.
fn cloud_state_label(cloud: &CloudCtx) -> (&'static str, Severity) {
    let mqtt = cloud
        .mqtt_state
        .clone()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let http = cloud
        .http_state
        .clone()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let has_drone = cloud
        .drone_id
        .as_deref()
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if !has_drone {
        return ("UNPAIRED", Severity::Muted);
    }
    if mqtt == "connected" && (http == "ok" || http == "connected") {
        return ("CONNECTED", Severity::Ok);
    }
    if matches!(mqtt.as_str(), "connecting" | "reconnecting")
        || matches!(http.as_str(), "connecting" | "reconnecting")
    {
        return ("RECONNECTING", Severity::Warn);
    }
    ("OFFLINE", Severity::Err)
}

/// The uplink detail view, registered as `details.uplink`.
pub struct UplinkDetailPage;

impl Page for UplinkDetailPage {
    fn id(&self) -> &'static str {
        "details.uplink"
    }

    fn refresh_hz(&self) -> f32 {
        2.0
    }

    fn render(&self, ctx: &PageContext, palette: &Palette) -> Canvas {
        let mut canvas = blank_panel(palette);
        draw_detail_header(&mut canvas, palette, "Uplink");

        render_cloud_band(&mut canvas, palette, &ctx.cloud);
        if ctx.uplink.modem_present {
            render_cellular_band(&mut canvas, palette, &ctx.uplink);
        } else {
            render_wifi_fallback(&mut canvas, palette, &ctx.network, &ctx.uplink);
        }
        canvas
    }

    fn hit_zones(&self, _ctx: &PageContext) -> Vec<HitZone> {
        vec![HitZone::new(8, 8, 40, 32, HitAction::Back)]
    }
}

/// Paint the top cloud-relay band: status badge + MQTT/HTTP/RTT + id/pair.
fn render_cloud_band(canvas: &mut Canvas, palette: &Palette, cloud: &CloudCtx) {
    let (label, severity) = cloud_state_label(cloud);
    let color = match severity {
        Severity::Ok => palette.status_success,
        Severity::Warn => palette.status_warning,
        Severity::Err => palette.status_error,
        Severity::Muted => palette.text_tertiary,
    };

    // Status dot (10 px disc centered at (17, CLOUD_BAND_Y + 11)).
    fill_circle(canvas, 17, CLOUD_BAND_Y + 11, 5, color, None);
    let title_font = LoadedFont::new(FontFace::SansBold, 14);
    text(
        canvas,
        &title_font,
        label,
        28,
        CLOUD_BAND_Y + 4,
        palette.text_primary,
    );

    let body_font = LoadedFont::new(FontFace::MonoRegular, 11);
    let mqtt = cloud.mqtt_state.clone().unwrap_or_else(|| "--".to_string());
    let http = cloud.http_state.clone().unwrap_or_else(|| "--".to_string());
    let rtt_text = match cloud.rtt_ms {
        Some(rtt) => format!("{} ms", rtt as i64),
        None => "-- ms".to_string(),
    };
    text(
        canvas,
        &body_font,
        &format!("mqtt {mqtt}"),
        12,
        CLOUD_BAND_Y + 28,
        palette.text_secondary,
    );
    text(
        canvas,
        &body_font,
        &format!("http {http}"),
        12,
        CLOUD_BAND_Y + 44,
        palette.text_secondary,
    );
    text(
        canvas,
        &body_font,
        &format!("rtt  {rtt_text}"),
        12,
        CLOUD_BAND_Y + 60,
        palette.text_secondary,
    );

    let drone_id = cloud.drone_id.clone().unwrap_or_default();
    let pairing_code = cloud.pairing_code.clone().unwrap_or_default();
    let right_x = 240;
    if !drone_id.is_empty() {
        text(
            canvas,
            &body_font,
            &format!("id {drone_id}"),
            right_x,
            CLOUD_BAND_Y + 28,
            palette.text_secondary,
        );
    }
    if !pairing_code.is_empty() && drone_id.is_empty() {
        let pair_font = LoadedFont::new(FontFace::MonoBold, 12);
        text(
            canvas,
            &pair_font,
            &format!("pair {pairing_code}"),
            right_x,
            CLOUD_BAND_Y + 44,
            palette.text_primary,
        );
    }
}

/// Paint the cellular band: signal bars + RSRP/RSRQ/SINR + band/IP/tech.
fn render_cellular_band(canvas: &mut Canvas, palette: &Palette, modem: &UplinkCtx) {
    let rsrp = modem.rsrp_dbm;
    let rsrq = modem.rsrq_db;
    let sinr = modem.sinr_db;
    let band = modem.band.clone().unwrap_or_else(|| "--".to_string());
    let ip = modem.ip.clone().unwrap_or_else(|| "--".to_string());
    let tech = modem
        .tech
        .clone()
        .unwrap_or_else(|| "--".to_string())
        .to_ascii_uppercase();
    let bars = bars_for_rsrp(rsrp);

    // Signal bars graphic at left (four ascending outlined rects).
    let bars_x = 12;
    let bars_y = CELL_BAND_Y + 8;
    let bar_w = 8;
    let bar_gap = 3;
    for i in 0..4 {
        let h = 8 + i * 6;
        let x0 = bars_x + i * (bar_w + bar_gap);
        let y0 = bars_y + (28 - h);
        let color = if i < bars {
            palette.accent_primary
        } else {
            palette.bg_tertiary
        };
        fill_rect_outline(
            canvas,
            x0,
            y0,
            x0 + bar_w - 1,
            bars_y + 28,
            color,
            palette.border_default,
        );
    }

    let body_font = LoadedFont::new(FontFace::MonoRegular, 11);
    let head_font = LoadedFont::new(FontFace::SansBold, 12);
    let rsrp_text = match rsrp {
        Some(v) => format!("rsrp {}", v as i64),
        None => "rsrp --".to_string(),
    };
    let rsrq_text = match rsrq {
        Some(v) => format!("rsrq {}", v as i64),
        None => "rsrq --".to_string(),
    };
    let sinr_text = match sinr {
        Some(v) => format!("sinr {}", v as i64),
        None => "sinr --".to_string(),
    };
    let text_x = bars_x + 4 * (bar_w + bar_gap) + 16;
    text(
        canvas,
        &head_font,
        &format!("{tech} · band {band}"),
        text_x,
        CELL_BAND_Y + 6,
        palette.text_primary,
    );
    text(
        canvas,
        &body_font,
        &format!("{rsrp_text} · {rsrq_text} · {sinr_text}"),
        text_x,
        CELL_BAND_Y + 22,
        palette.text_secondary,
    );
    text(
        canvas,
        &body_font,
        &format!("ip {ip}"),
        text_x,
        CELL_BAND_Y + 38,
        palette.text_secondary,
    );
}

/// Paint the WiFi-client fallback (or a no-WAN notice) when no modem is up.
fn render_wifi_fallback(
    canvas: &mut Canvas,
    palette: &Palette,
    network: &NetworkCtx,
    modem: &UplinkCtx,
) {
    let wifi = &network.wifi_client;
    let body_font = LoadedFont::new(FontFace::MonoRegular, 12);
    let head_font = LoadedFont::new(FontFace::SansBold, 12);
    if wifi.connected {
        let ssid = wifi.ssid.clone().unwrap_or_else(|| "--".to_string());
        let sig_text = match wifi.signal_dbm {
            Some(s) => format!("signal {} dBm", s as i64),
            None => "signal --".to_string(),
        };
        text(
            canvas,
            &head_font,
            &format!("WiFi uplink: {ssid}"),
            12,
            CELL_BAND_Y + 12,
            palette.text_primary,
        );
        text(
            canvas,
            &body_font,
            &sig_text,
            12,
            CELL_BAND_Y + 30,
            palette.text_secondary,
        );
    } else {
        text(
            canvas,
            &head_font,
            "No WAN uplink",
            12,
            CELL_BAND_Y + 14,
            palette.text_tertiary,
        );
        if let Some(reason) = modem.reason.as_deref() {
            if !reason.is_empty() {
                text(
                    canvas,
                    &body_font,
                    &format!("modem: {reason}"),
                    12,
                    CELL_BAND_Y + 32,
                    palette.text_tertiary,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;
    use crate::pages::PANEL_W;

    #[test]
    fn uplink_renders_with_back_zone() {
        let page = UplinkDetailPage;
        let ctx = PageContext::default();
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
        let zones = page.hit_zones(&ctx);
        assert_eq!(zones.len(), 1);
        assert_eq!(zones[0].action, HitAction::Back);
    }

    #[test]
    fn cloud_label_buckets() {
        let mut cloud = CloudCtx::default();
        // No drone id -> unpaired.
        assert_eq!(cloud_state_label(&cloud), ("UNPAIRED", Severity::Muted));
        cloud.drone_id = Some("ados-58c27faf".to_string());
        cloud.mqtt_state = Some("connected".to_string());
        cloud.http_state = Some("ok".to_string());
        assert_eq!(cloud_state_label(&cloud), ("CONNECTED", Severity::Ok));
        cloud.mqtt_state = Some("reconnecting".to_string());
        assert_eq!(cloud_state_label(&cloud), ("RECONNECTING", Severity::Warn));
        cloud.mqtt_state = Some("error".to_string());
        cloud.http_state = Some("down".to_string());
        assert_eq!(cloud_state_label(&cloud), ("OFFLINE", Severity::Err));
    }

    #[test]
    fn rsrp_to_bars() {
        assert_eq!(bars_for_rsrp(None), 0);
        assert_eq!(bars_for_rsrp(Some(-85.0)), 4);
        assert_eq!(bars_for_rsrp(Some(-95.0)), 3);
        assert_eq!(bars_for_rsrp(Some(-105.0)), 2);
        assert_eq!(bars_for_rsrp(Some(-120.0)), 1);
    }

    #[test]
    fn cellular_band_renders_when_modem_present() {
        let page = UplinkDetailPage;
        let mut ctx = PageContext::default();
        ctx.uplink.modem_present = true;
        ctx.uplink.rsrp_dbm = Some(-92.0);
        ctx.uplink.rsrq_db = Some(-11.0);
        ctx.uplink.sinr_db = Some(8.0);
        ctx.uplink.band = Some("3".to_string());
        ctx.uplink.ip = Some("10.64.0.2".to_string());
        ctx.uplink.tech = Some("lte".to_string());
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
    }

    #[test]
    fn wifi_fallback_renders_when_no_modem() {
        let page = UplinkDetailPage;
        let mut ctx = PageContext::default();
        ctx.network.wifi_client.connected = true;
        ctx.network.wifi_client.ssid = Some("Ajay & Nidhi".to_string());
        ctx.network.wifi_client.signal_dbm = Some(-47.0);
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
    }
}
