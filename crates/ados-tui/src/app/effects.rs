//! Post-render effects: hook the frame's rects up to motion (tab fade,
//! overlay fade, row flashes, toast in/out, chip pulses, the intro sweep).

use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::{App, FrameOut};

/// How long before expiry a toast starts fading out.
const TOAST_OUT: std::time::Duration = std::time::Duration::from_millis(200);

impl App {
    /// After a frame is drawn: record hits, start effects whose rects are now
    /// known, and run every effect over the buffer.
    pub fn post_render(&mut self, out: FrameOut, buf: &mut Buffer, area: Rect) {
        let core = &mut self.core;
        let canvas = core.theme.canvas_or_reset();
        core.last_hits = out.hits;
        core.last_body = out.body;
        if std::mem::take(&mut core.pending_tab_fx) {
            core.fx.tab_switch(out.body, canvas);
        }
        if std::mem::take(&mut core.pending_overlay_fx) {
            if let Some(r) = out.overlay {
                core.fx.overlay(r, canvas);
            }
        }
        for (key, ok) in std::mem::take(&mut core.flashes) {
            if let Some((_, rect)) = out.rows.iter().find(|(k, _)| *k == key) {
                if ok {
                    core.fx.applied(*rect, core.theme.accent());
                } else {
                    core.fx.failed(*rect, core.theme.error());
                }
            }
        }
        let expiring: Vec<(u64, std::time::Instant)> = core
            .toasts
            .visible(core.now)
            .iter()
            .map(|t| (t.id, t.expires()))
            .collect();
        for (id, rect) in &out.toasts {
            if !core.toasts_in.contains(id) {
                core.toasts_in.push(*id);
                core.fx.toast_in(*rect, canvas);
            }
            // Fade out over the last 200 ms of the toast's life.
            let ending = expiring
                .iter()
                .any(|(i, at)| i == id && at.saturating_duration_since(core.now) <= TOAST_OUT);
            if ending && !core.toasts_out.contains(id) {
                core.toasts_out.push(*id);
                core.fx.toast_out(*rect, canvas);
            }
        }
        for list in [&mut core.toasts_in, &mut core.toasts_out] {
            if list.len() > 64 {
                let drop = list.len() - 64;
                list.drain(..drop);
            }
        }
        let armed = core.dash().and_then(|d| d.armed) == Some(true) && !core.stale();
        match out.armed_chip {
            Some(r) => core.fx.armed_pulse(r, armed),
            None => core.fx.armed_pulse(Rect::default(), false),
        }
        match out.offline_chip {
            Some(r) => core.fx.offline_pulse(r, true),
            None => core.fx.offline_pulse(Rect::default(), false),
        }
        if !core.had_data && core.view.dash.is_some() {
            core.had_data = true;
            core.fx.intro(area, canvas);
        }
        core.fx.process(buf, area);
        core.frame = core.frame.wrapping_add(1);
        core.dirty = false;
    }

    /// Whether the screen needs redrawing without new input.
    pub fn needs_frame(&self) -> bool {
        self.core.dirty
            || self.core.fx.running()
            || self.tabs.get(self.core.tab, self.core.profile()).animating(&self.core)
            || self
                .core
                .toasts
                .next_expiry()
                .is_some_and(|t| t.saturating_duration_since(Instant::now()) <= TOAST_OUT * 2)
    }
}
