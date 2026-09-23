//! Settings page — the node's current configuration at a glance.
//!
//! The top-level Settings tab. It renders a column of 48 px rows, one per
//! setting area: the radio link (channel and TX power), the mesh role, the
//! access point, the uplink, and the agent build. Each row shows the current
//! value read from the same live [`PageContext`] every other page reads, and
//! drills into the detail page that owns that setting (the radio page carries
//! the TX-power steppers, the mesh page the role picker). A value the agent does
//! not report reads `—`, never a default.

use embedded_graphics::pixelcolor::Rgb888;

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_rect, line, text, Canvas};
use crate::pages::{
    blank_panel, Chrome, HitAction, HitZone, Page, PageContext, CONTENT_H, CONTENT_Y, PANEL_W,
};
use crate::widgets::{bottom_bar_zones, draw_bottom_bar, draw_top_bar};

/// Height of one settings list row.
const ROW_H: i32 = 48;

/// Pixel padding from the left edge for the label.
const LEFT_PAD: i32 = 12;
/// Pixel padding from the right edge for the chevron.
const RIGHT_PAD: i32 = 12;

/// Shown for a value the agent does not report.
const NO_VALUE: &str = "—";

/// One settings row: its label, the current value, and the detail page that
/// owns the setting.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsRow {
    pub label: &'static str,
    pub value: String,
    pub target: &'static str,
}

/// The rows, resolved from the live context. Five rows fill the 244 px content
/// region exactly (`5 * 48 = 240`), so the list never scrolls.
pub fn settings_rows(ctx: &PageContext) -> [SettingsRow; 5] {
    let link = &ctx.link;
    let radio = if link.is_stale() {
        "stale".to_string()
    } else {
        match (link.channel.filter(|c| *c > 0), link.tx_power_dbm) {
            (Some(ch), Some(tx)) => format!("ch {ch} · {tx} dBm"),
            (Some(ch), None) => format!("ch {ch}"),
            (None, Some(tx)) => format!("{tx} dBm"),
            (None, None) => NO_VALUE.to_string(),
        }
    };
    let or_none = |v: Option<&str>| v.unwrap_or(NO_VALUE).to_string();
    let uplink = match ctx.network.uplink_type.as_deref() {
        Some(kind) if kind != "none" => kind.to_string(),
        Some(_) => "none".to_string(),
        None => NO_VALUE.to_string(),
    };
    [
        SettingsRow {
            label: "Radio link",
            value: radio,
            target: "details.radio_link",
        },
        SettingsRow {
            label: "Mesh role",
            value: or_none(ctx.role.current.as_deref()),
            target: "details.mesh",
        },
        SettingsRow {
            label: "Access point",
            value: or_none(
                ctx.network
                    .ap_ssid
                    .as_deref()
                    .or(ctx.network.hotspot_ssid.as_deref()),
            ),
            target: "details.access_point",
        },
        SettingsRow {
            label: "Uplink",
            value: uplink,
            target: "details.uplink",
        },
        SettingsRow {
            label: "About",
            value: ctx
                .device
                .version
                .as_deref()
                .map(|v| format!("v{v}"))
                .unwrap_or_else(|| NO_VALUE.to_string()),
            target: "details.about",
        },
    ]
}

/// The settings list, registered as `settings`.
pub struct SettingsPage;

impl Page for SettingsPage {
    fn id(&self) -> &'static str {
        "settings"
    }

    fn chrome(&self) -> Chrome {
        Chrome::Tabbed
    }

    fn refresh_hz(&self) -> f32 {
        2.0
    }

    fn render(&self, ctx: &PageContext, palette: &Palette) -> Canvas {
        let mut canvas = blank_panel(palette);
        draw_top_bar(
            &mut canvas,
            palette,
            &ctx.hostname,
            ctx.role.current.as_deref().unwrap_or("unset"),
            ctx.system.cpu_pct,
            ctx.system.ram_used_mb,
            ctx.system.ram_total_mb,
            ctx.system.temp_c,
            &ctx.clock,
        );
        render_content(&mut canvas, palette, ctx);
        draw_bottom_bar(&mut canvas, palette, self.id());
        canvas
    }

    fn hit_zones(&self, ctx: &PageContext) -> Vec<HitZone> {
        let mut zones: Vec<HitZone> = settings_rows(ctx)
            .iter()
            .enumerate()
            .map(|(i, row)| {
                HitZone::new(
                    0,
                    i as i32 * ROW_H,
                    PANEL_W as i32,
                    ROW_H,
                    HitAction::OpenDetail(row.target),
                )
            })
            .collect();
        zones.extend(bottom_bar_zones());
        zones
    }
}

/// Paint the row list into the content region. Page-local coordinates shift
/// down by [`CONTENT_Y`] for the panel-global paint.
fn render_content(canvas: &mut Canvas, palette: &Palette, ctx: &PageContext) {
    let oy = CONTENT_Y as i32;
    // Clear the content region so a redraw never keeps a previous page's pixels.
    fill_rect(
        canvas,
        0,
        oy,
        PANEL_W as i32 - 1,
        oy + CONTENT_H as i32 - 1,
        palette.bg_primary,
    );
    for (i, row) in settings_rows(ctx).iter().enumerate() {
        draw_list_row(canvas, palette, oy + i as i32 * ROW_H, row);
    }
}

/// Paint one 48 px list row at panel-global `y`: the label, the current value,
/// a chevron into the owning detail page, and a 1 px divider.
fn draw_list_row(canvas: &mut Canvas, palette: &Palette, y: i32, row: &SettingsRow) {
    let label_font = LoadedFont::new(FontFace::SansRegular, 14);
    let (_, label_h) = label_font.text_size(row.label);
    text(
        canvas,
        &label_font,
        row.label,
        LEFT_PAD,
        y + (ROW_H - label_h as i32) / 2 - 2,
        palette.text_primary,
    );

    let chevron_x = PANEL_W as i32 - RIGHT_PAD - 8;
    let value_font = LoadedFont::new(FontFace::MonoRegular, 12);
    let (vw, vh) = value_font.text_size(&row.value);
    let value_color = if row.value == NO_VALUE || row.value == "stale" {
        palette.text_tertiary
    } else {
        palette.text_secondary
    };
    text(
        canvas,
        &value_font,
        &row.value,
        chevron_x - 12 - vw as i32,
        y + (ROW_H - vh as i32) / 2 - 1,
        value_color,
    );
    draw_chevron(canvas, chevron_x, y + ROW_H / 2, palette.text_tertiary);

    line(
        canvas,
        0,
        y + ROW_H - 1,
        PANEL_W as i32 - 1,
        y + ROW_H - 1,
        palette.border_default,
    );
}

/// Draw a right-pointing chevron centered on `(cx, cy)`.
fn draw_chevron(canvas: &mut Canvas, cx: i32, cy: i32, color: Rgb888) {
    let arm = 5;
    // Two-pixel stroke, one row apart, so the chevron reads on the panel.
    for off in 0..2 {
        line(canvas, cx - arm, cy - arm + off, cx, cy + off, color);
        line(canvas, cx, cy + off, cx - arm, cy + arm + off, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;

    /// The tab is never blank: every row paints from the live context, with
    /// `—` for what the agent does not report.
    #[test]
    fn rows_come_from_the_live_context() {
        let mut ctx = PageContext::default();
        let rows = settings_rows(&ctx);
        assert!(rows.iter().all(|r| r.value == NO_VALUE), "{rows:?}");

        ctx.link.channel = Some(149);
        ctx.link.tx_power_dbm = Some(10);
        ctx.role.current = Some("relay".to_string());
        ctx.network.ap_ssid = Some("ADOS-GS".to_string());
        ctx.network.uplink_type = Some("eth".to_string());
        ctx.device.version = Some("1.2.3".to_string());
        let values: Vec<String> = settings_rows(&ctx).into_iter().map(|r| r.value).collect();
        assert_eq!(
            values,
            ["ch 149 · 10 dBm", "relay", "ADOS-GS", "eth", "v1.2.3"]
        );

        ctx.link.state = Some("stale".to_string());
        assert_eq!(settings_rows(&ctx)[0].value, "stale");
    }

    #[test]
    fn each_row_opens_the_page_that_owns_the_setting() {
        let page = SettingsPage;
        let ctx = PageContext::default();
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
        let zones = page.hit_zones(&ctx);
        let targets: Vec<HitAction> = zones.iter().take(5).map(|z| z.action.clone()).collect();
        assert_eq!(
            targets,
            [
                HitAction::OpenDetail("details.radio_link"),
                HitAction::OpenDetail("details.mesh"),
                HitAction::OpenDetail("details.access_point"),
                HitAction::OpenDetail("details.uplink"),
                HitAction::OpenDetail("details.about"),
            ]
        );
        // Five rows then the five tabs.
        assert_eq!(zones.len(), 10);
        assert!(5 * ROW_H <= CONTENT_H as i32);
    }
}
