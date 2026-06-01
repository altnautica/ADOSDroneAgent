//! Channel Hops page — the channel-hopping history chart.
//!
//! A top-level tab and a read-only watch surface. It renders the recent hop
//! history from [`PageContext::hopping`] as a step-after line plus scatter
//! markers colored by trigger (periodic green, reactive amber) and outcome
//! (failed red), with a dashed reference line at the current radio channel, a
//! header strip (title + band + hop count), and a legend strip (color key plus
//! the last-hop summary).
//!
//! The content region is a 480x244 frame that sits below the persistent top
//! status bar, so every page-local y derives from [`CONTENT_Y`]. The page is a
//! pure composer: no tap drilldowns, only the bottom-bar tab zones.

use std::time::{SystemTime, UNIX_EPOCH};

use embedded_graphics::pixelcolor::Rgb888;

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_circle, fill_rect, fill_rect_outline, line, text, Canvas};
use crate::pages::{
    blank_panel, HitZone, HopEntry, Page, PageContext, CONTENT_H, CONTENT_Y, PANEL_W,
};
use crate::widgets::{bottom_bar_zones, draw_bottom_bar, draw_top_bar};

/// Content-region width.
const PAGE_W: i32 = PANEL_W as i32;
/// Content-region height (the 480x244 frame below the top status bar).
const PAGE_H: i32 = CONTENT_H as i32;
/// Panel-global y offset of the content region's top edge.
const OY: i32 = CONTENT_Y as i32;

/// Header strip height.
const HEADER_H: i32 = 24;
/// Legend strip height.
const LEGEND_H: i32 = 30;
/// Chart band height (the remainder between header and legend).
const CHART_H: i32 = PAGE_H - HEADER_H - LEGEND_H;

/// Chart inner padding.
const CHART_LEFT_PAD: i32 = 36;
const CHART_RIGHT_PAD: i32 = 12;
const CHART_TOP_PAD: i32 = 8;
const CHART_BOTTOM_PAD: i32 = 22;

/// Y-axis breathing room above / below the channel extrema.
const Y_PAD_CHANNELS: i64 = 4;

/// Cap on how many of the most recent hops the chart walks.
const MAX_HOPS: usize = 32;

/// The channel-hopping history surface, registered as `channel_hops`.
pub struct ChannelHopsPage;

impl Page for ChannelHopsPage {
    fn id(&self) -> &'static str {
        "channel_hops"
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
        // Ground the content region.
        fill_rect(
            &mut canvas,
            0,
            OY,
            PAGE_W - 1,
            OY + PAGE_H - 1,
            palette.bg_primary,
        );

        let history = recent_history(&ctx.hopping.history);
        let radio_channel = ctx.hopping.radio_channel;

        draw_header(&mut canvas, palette, &ctx.hopping.band, history.len());
        if history.is_empty() {
            draw_empty(&mut canvas, palette, radio_channel);
        } else {
            draw_chart(&mut canvas, palette, history, radio_channel);
            draw_legend(&mut canvas, palette, history);
        }

        draw_bottom_bar(&mut canvas, palette, self.id());
        canvas
    }

    fn hit_zones(&self, _ctx: &PageContext) -> Vec<HitZone> {
        bottom_bar_zones()
    }
}

/// The most recent hops, capped at [`MAX_HOPS`].
fn recent_history(history: &[HopEntry]) -> &[HopEntry] {
    let from = history.len().saturating_sub(MAX_HOPS);
    &history[from..]
}

/// Current wall-clock as seconds since the Unix epoch, for the "-Ns" labels.
fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The band label, or an em dash when the supervisor has not reported one.
fn band_label(band: &Option<String>) -> String {
    match band {
        Some(b) if !b.is_empty() => b.clone(),
        _ => "—".to_string(),
    }
}

/// Paint the header strip: a caps title on the left and a band + hop-count
/// summary on the right, over a divider.
fn draw_header(canvas: &mut Canvas, palette: &Palette, band: &Option<String>, hops: usize) {
    fill_rect(
        canvas,
        0,
        OY,
        PAGE_W - 1,
        OY + HEADER_H - 1,
        palette.bg_secondary,
    );
    line(
        canvas,
        0,
        OY + HEADER_H - 1,
        PAGE_W - 1,
        OY + HEADER_H - 1,
        palette.border_default,
    );
    let title_f = LoadedFont::new(FontFace::SansBold, 11);
    text(
        canvas,
        &title_f,
        "CHANNEL HOPS",
        12,
        OY + 6,
        palette.text_tertiary,
    );
    let right = format!("{} · {} hops", band_label(band), hops);
    text(
        canvas,
        &title_f,
        &right,
        PAGE_W - 150,
        OY + 6,
        palette.text_secondary,
    );
}

/// Paint the empty-state body: a "no hops yet" headline and a sub-line that
/// reports the current channel (or that the supervisor is armed).
fn draw_empty(canvas: &mut Canvas, palette: &Palette, radio_channel: Option<i64>) {
    let big_f = LoadedFont::new(FontFace::SansBold, 14);
    let small_f = LoadedFont::new(FontFace::SansRegular, 11);
    let msg = "No hops yet";
    let sub = match radio_channel {
        Some(ch) => format!("current channel {ch}"),
        None => "supervisor is armed".to_string(),
    };
    text(
        canvas,
        &big_f,
        msg,
        PAGE_W / 2 - 50,
        OY + HEADER_H + CHART_H / 2 - 16,
        palette.text_primary,
    );
    text(
        canvas,
        &small_f,
        &sub,
        PAGE_W / 2 - 80,
        OY + HEADER_H + CHART_H / 2 + 4,
        palette.text_tertiary,
    );
}

/// Paint the chart band: axes + tick labels + dashed reference line + the
/// step-after path through the hops + a scatter marker per hop.
fn draw_chart(
    canvas: &mut Canvas,
    palette: &Palette,
    history: &[HopEntry],
    radio_channel: Option<i64>,
) {
    let chart_x0 = CHART_LEFT_PAD;
    let chart_y0 = OY + HEADER_H + CHART_TOP_PAD;
    let chart_x1 = PAGE_W - CHART_RIGHT_PAD;
    let chart_y1 = OY + HEADER_H + CHART_H - CHART_BOTTOM_PAD;
    let chart_w = chart_x1 - chart_x0;
    let chart_h = chart_y1 - chart_y0;

    // Y-axis domain: the hop "to" channels plus the live reference channel.
    let mut ys: Vec<i64> = history.iter().map(|e| e.to_channel).collect();
    if let Some(ch) = radio_channel {
        ys.push(ch);
    }
    let raw_min = ys.iter().copied().min().unwrap_or(1);
    let raw_max = ys.iter().copied().max().unwrap_or(1);
    let y_min = (raw_min - Y_PAD_CHANNELS).max(1);
    let mut y_max = (raw_max + Y_PAD_CHANNELS).min(165);
    if y_max <= y_min {
        y_max = y_min + 1;
    }

    // X-axis domain: seconds from the oldest entry.
    let t0 = history[0].at;
    let t_last = history[history.len() - 1].at;
    let x_span = (t_last - t0).max(1.0);

    // A hop record whose timestamps run newest-first (or any out-of-order /
    // sparse history) would drive `tx` far outside `0..=1`. PIL silently clips
    // off-canvas coordinates; the panel rasterizer instead walks every pixel of
    // the segment, so an unclamped coordinate billions of pixels off-screen
    // would blow the line algorithm. Clamp the normalized fractions to the
    // drawable band so the chart degrades to an edge-pinned point exactly where
    // PIL would clip it, and never feeds the rasterizer a runaway endpoint.
    let to_px = |at: f64, ch: f64| -> (i32, i32) {
        let tx = ((at - t0) / x_span).clamp(0.0, 1.0);
        let ty = ((ch - y_min as f64) / (y_max - y_min) as f64).clamp(0.0, 1.0);
        let px = chart_x0 + (tx * chart_w as f64) as i32;
        let py = chart_y1 - (ty * chart_h as f64) as i32;
        (px, py)
    };

    // Chart background.
    fill_rect_outline(
        canvas,
        chart_x0 - 1,
        chart_y0 - 1,
        chart_x1 + 1,
        chart_y1 + 1,
        palette.bg_secondary,
        palette.border_default,
    );

    // Y-axis tick labels at min, mid, max.
    let axis_f = LoadedFont::new(FontFace::MonoRegular, 9);
    for ch in [y_min, (y_min + y_max) / 2, y_max] {
        let (_, py) = to_px(t0, ch as f64);
        text(
            canvas,
            &axis_f,
            &ch.to_string(),
            4,
            py - 6,
            palette.text_tertiary,
        );
        line(
            canvas,
            chart_x0 - 2,
            py,
            chart_x0,
            py,
            palette.border_default,
        );
    }

    // X-axis tick labels: oldest (left), midpoint, newest (right).
    let now_s = now_seconds();
    let ticks = [
        (t0, chart_x0),
        ((t0 + t_last) / 2.0, (chart_x0 + chart_x1) / 2),
        (t_last, chart_x1 - 30),
    ];
    for (t_at, anchor_x) in ticks {
        let delta = (now_s - t_at) as i64;
        let label = if delta <= 1 {
            "now".to_string()
        } else {
            format!("-{delta}s")
        };
        text(
            canvas,
            &axis_f,
            &label,
            anchor_x,
            chart_y1 + 4,
            palette.text_tertiary,
        );
    }

    // Dashed reference line at the current channel, when inside the visible
    // range.
    if let Some(ch) = radio_channel {
        if y_min <= ch && ch <= y_max {
            let (_, ref_py) = to_px(t0, ch as f64);
            draw_dashed_hline(canvas, chart_x0, ref_py, chart_x1, palette.accent_primary);
        }
    }

    // Step-after path: each hop emits a horizontal run at the previous channel
    // and a vertical jump to its new channel.
    let line_color = palette.text_secondary;
    let mut prev: Option<(i32, i32)> = None;
    for entry in history {
        let (from_px, from_py) = to_px(entry.at, entry.from_channel as f64);
        let (_, to_py) = to_px(entry.at, entry.to_channel as f64);
        if let Some((prev_px, prev_py)) = prev {
            line(canvas, prev_px, prev_py, from_px, prev_py, line_color);
            if prev_py != from_py {
                line(canvas, from_px, prev_py, from_px, from_py, line_color);
            }
        }
        line(canvas, from_px, from_py, from_px, to_py, line_color);
        prev = Some((from_px, to_py));
    }
    // Extend the last step to the right edge — "we are on this channel now".
    if let Some((prev_px, prev_py)) = prev {
        if prev_px < chart_x1 {
            line(canvas, prev_px, prev_py, chart_x1, prev_py, line_color);
        }
    }

    // Scatter markers per hop, colored by trigger + outcome.
    for entry in history {
        let trigger = entry.trigger.as_deref().unwrap_or("periodic");
        let color = marker_color(palette, trigger, entry.ok);
        let (cx, cy) = to_px(entry.at, entry.to_channel as f64);
        fill_circle(canvas, cx, cy, 3, color, Some(palette.bg_primary));
    }
}

/// The scatter-marker color: red for a failed hop, amber for a reactive hop,
/// green for a successful periodic hop.
fn marker_color(palette: &Palette, trigger: &str, ok: bool) -> Rgb888 {
    if !ok {
        palette.status_error
    } else if trigger == "reactive" {
        palette.status_warning
    } else {
        palette.status_success
    }
}

/// Stroke a dashed horizontal line at `y` from `x0` to `x1`.
fn draw_dashed_hline(canvas: &mut Canvas, x0: i32, y: i32, x1: i32, color: Rgb888) {
    let seg = 4;
    let gap = 3;
    let mut x = x0;
    while x < x1 {
        line(canvas, x, y, (x + seg).min(x1), y, color);
        x += seg + gap;
    }
}

/// Paint the legend strip: three colored dots with labels on the left and the
/// last-hop summary on the right.
fn draw_legend(canvas: &mut Canvas, palette: &Palette, history: &[HopEntry]) {
    let legend_y = OY + HEADER_H + CHART_H;
    fill_rect(
        canvas,
        0,
        legend_y,
        PAGE_W - 1,
        OY + PAGE_H - 1,
        palette.bg_secondary,
    );
    line(
        canvas,
        0,
        legend_y,
        PAGE_W - 1,
        legend_y,
        palette.border_default,
    );
    let f = LoadedFont::new(FontFace::SansRegular, 10);

    let items = [
        (palette.status_success, "periodic"),
        (palette.status_warning, "reactive"),
        (palette.status_error, "failed"),
    ];
    let mut cx = 14;
    let cy = legend_y + 14;
    for (color, label) in items {
        fill_circle(canvas, cx, cy, 4, color, None);
        text(canvas, &f, label, cx + 8, cy - 6, palette.text_primary);
        cx += 90;
    }

    // Right side: "last: -Ns (from -> to)".
    let last = &history[history.len() - 1];
    let delta = (now_seconds() - last.at) as i64;
    let summary = format!(
        "last: -{}s ({} -> {})",
        delta.max(0),
        last.from_channel,
        last.to_channel
    );
    text(
        canvas,
        &f,
        &summary,
        PAGE_W - 170,
        cy - 6,
        palette.text_secondary,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;

    fn hop(at: f64, from: i64, to: i64, ok: bool, trigger: &str) -> HopEntry {
        HopEntry {
            at,
            from_channel: from,
            to_channel: to,
            ok,
            trigger: Some(trigger.to_string()),
        }
    }

    #[test]
    fn empty_history_renders_with_tab_zones() {
        let page = ChannelHopsPage;
        let ctx = PageContext::default();
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
        assert_eq!(page.hit_zones(&ctx).len(), 5);
        assert_eq!(page.id(), "channel_hops");
    }

    #[test]
    fn populated_history_renders_chart_and_legend() {
        let page = ChannelHopsPage;
        let mut ctx = PageContext::default();
        ctx.hopping.band = Some("u-nii-3".to_string());
        ctx.hopping.radio_channel = Some(149);
        let base = 1_700_000_000.0;
        ctx.hopping.history = vec![
            hop(base, 149, 153, true, "periodic"),
            hop(base + 30.0, 153, 157, true, "reactive"),
            hop(base + 60.0, 157, 149, false, "reactive"),
        ];
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
    }

    #[test]
    fn history_is_capped_at_max_hops() {
        let base = 1_700_000_000.0;
        let many: Vec<HopEntry> = (0..50)
            .map(|i| hop(base + i as f64, 149, 153, true, "periodic"))
            .collect();
        let recent = recent_history(&many);
        assert_eq!(recent.len(), MAX_HOPS);
        // The tail is kept: the last entry survives the cap.
        assert_eq!(recent[recent.len() - 1].at, base + 49.0);
    }

    #[test]
    fn reversed_history_renders_without_overflow() {
        // A history whose timestamps run newest-first drives the time fraction
        // far outside 0..=1; the chart must clamp to the drawable band rather
        // than feed the line rasterizer a runaway endpoint.
        let page = ChannelHopsPage;
        let mut ctx = PageContext::default();
        ctx.hopping.radio_channel = Some(149);
        let base = 1_700_000_000.0;
        ctx.hopping.history = vec![
            hop(base + 120.0, 157, 149, true, "reactive"),
            hop(base + 60.0, 153, 157, true, "periodic"),
            hop(base, 149, 153, false, "reactive"),
        ];
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
        assert_eq!(c.height(), crate::pages::PANEL_H);
    }

    #[test]
    fn marker_color_follows_trigger_and_outcome() {
        assert_eq!(marker_color(&DARK, "periodic", false), DARK.status_error);
        assert_eq!(marker_color(&DARK, "reactive", true), DARK.status_warning);
        assert_eq!(marker_color(&DARK, "periodic", true), DARK.status_success);
    }
}
