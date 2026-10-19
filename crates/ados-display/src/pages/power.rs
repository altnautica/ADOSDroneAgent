//! Power page — the power-off confirmation.
//!
//! A `shutdown_prompt` button press pushes this page as a modal. It asks
//! "Power off?" with two touch targets: Power off sends
//! `POST /api/v1/setup/shutdown` through the panel's agent writer, Cancel pops
//! the modal. A second `shutdown_prompt` press while the page is on top
//! confirms the same write, and the `back` button cancels, so a panel without
//! touch can answer it too. The hint names the buttons the live mapping binds
//! to those two actions, so a remapped panel reads its own keys.

use embedded_graphics::pixelcolor::Rgb888;

use crate::button_source::ButtonLayout;
use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_rect, fill_rect_outline, text, Canvas};
use crate::pages::{
    blank_panel, AgentRequest, Chrome, HitAction, HitZone, Page, PageContext, PanelAction, PANEL_W,
};

/// The route id the navigator pushes for a `shutdown_prompt` press.
pub const POWER_PAGE_ID: &str = "details.power";
/// The custom key the Power off target (and a confirming press) resolves.
pub const POWER_OFF_KEY: &str = "power.off";
/// The agent route that powers the node off.
pub const SHUTDOWN_PATH: &str = "/api/v1/setup/shutdown";

/// The prompt line.
const TITLE: &str = "Power off?";
/// The hint when no button is bound to the prompt: touch only.
const TOUCH_HINT: &str = "Tap Power off to confirm";
/// Top edge of the prompt line.
const TITLE_Y: i32 = 64;
/// Top edge of the hint line.
const HINT_Y: i32 = 116;
/// Top edge of the target row.
const BTN_Y: i32 = 188;
/// Width of each target.
const BTN_W: i32 = 200;
/// Height of each target.
const BTN_H: i32 = 64;
/// Left edge of the Power off target.
const POWER_BTN_X: i32 = 24;
/// Left edge of the Cancel target.
const CANCEL_BTN_X: i32 = PANEL_W as i32 - 24 - BTN_W;

/// The power-off confirmation, registered as `details.power`.
pub struct PowerPage {
    hint: String,
}

impl PowerPage {
    /// The page for the panel's button mapping.
    pub fn new(buttons: &ButtonLayout) -> Self {
        Self {
            hint: power_hint(
                buttons.key_for(ados_protocol::panel_ui::SHUTDOWN_PROMPT),
                buttons.key_for("back"),
            ),
        }
    }
}

/// `B1_long` → `("B1", true)`.
fn split_key(key: &str) -> (&str, bool) {
    match key.rsplit_once('_') {
        Some((button, kind)) => (button, kind == "long"),
        None => (key, false),
    }
}

/// The answer hint for the mapping keys bound to `shutdown_prompt` and
/// `back`: "Hold B1 again to power off, press B4 to cancel". Without a
/// prompt key the answer is touch only.
pub fn power_hint(confirm: Option<&str>, back: Option<&str>) -> String {
    let mut hint = match confirm.map(split_key) {
        Some((button, true)) => format!("Hold {button} again to power off"),
        Some((button, false)) => format!("Press {button} again to power off"),
        None => TOUCH_HINT.to_string(),
    };
    match back.map(split_key) {
        Some((button, true)) => hint.push_str(&format!(", hold {button} to cancel")),
        Some((button, false)) => hint.push_str(&format!(", press {button} to cancel")),
        None => {}
    }
    hint
}

impl Page for PowerPage {
    fn id(&self) -> &'static str {
        POWER_PAGE_ID
    }

    fn chrome(&self) -> Chrome {
        Chrome::FullScreen
    }

    fn refresh_hz(&self) -> f32 {
        1.0
    }

    fn render(&self, _ctx: &PageContext, palette: &Palette) -> Canvas {
        let mut canvas = blank_panel(palette);
        let title_font = LoadedFont::new(FontFace::SansBold, 28);
        let title = (TITLE, 0, PANEL_W as i32, TITLE_Y);
        centered(&mut canvas, &title_font, title, palette.text_primary);
        let hint_font = LoadedFont::new(FontFace::SansRegular, 14);
        let hint = (self.hint.as_str(), 0, PANEL_W as i32, HINT_Y);
        centered(&mut canvas, &hint_font, hint, palette.text_secondary);

        fill_rect(
            &mut canvas,
            POWER_BTN_X,
            BTN_Y,
            POWER_BTN_X + BTN_W - 1,
            BTN_Y + BTN_H - 1,
            palette.status_error,
        );
        fill_rect_outline(
            &mut canvas,
            CANCEL_BTN_X,
            BTN_Y,
            CANCEL_BTN_X + BTN_W - 1,
            BTN_Y + BTN_H - 1,
            palette.bg_secondary,
            palette.border_default,
        );
        let btn_font = LoadedFont::new(FontFace::SansBold, 16);
        let (_, bh) = btn_font.text_size("Power off");
        let label_y = BTN_Y + (BTN_H - bh as i32) / 2 - 1;
        let power = ("Power off", POWER_BTN_X, BTN_W, label_y);
        centered(&mut canvas, &btn_font, power, palette.text_primary);
        let cancel = ("Cancel", CANCEL_BTN_X, BTN_W, label_y);
        centered(&mut canvas, &btn_font, cancel, palette.text_primary);
        canvas
    }

    fn hit_zones(&self, _ctx: &PageContext) -> Vec<HitZone> {
        vec![
            HitZone::new(
                POWER_BTN_X,
                BTN_Y,
                BTN_W,
                BTN_H,
                HitAction::Custom(POWER_OFF_KEY.to_string()),
            ),
            HitZone::new(CANCEL_BTN_X, BTN_Y, BTN_W, BTN_H, HitAction::Back),
        ]
    }

    fn on_custom(&self, key: &str, _ctx: &PageContext) -> Option<PanelAction> {
        (key == POWER_OFF_KEY).then(|| {
            PanelAction::Agent(AgentRequest {
                method: "POST",
                path: SHUTDOWN_PATH,
                body: None,
                label: "Power off".to_string(),
            })
        })
    }
}

/// Paint `(label, x, w, y)`: `label` horizontally centred in `[x, x + w)` with
/// its top edge at `y`.
fn centered(
    canvas: &mut Canvas,
    font: &LoadedFont,
    (label, x, w, y): (&str, i32, i32, i32),
    color: Rgb888,
) {
    let (tw, _) = font.text_size(label);
    text(canvas, font, label, x + (w - tw as i32) / 2, y, color);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;

    #[test]
    fn power_off_target_sends_the_shutdown_write() {
        let page = PowerPage::new(&ButtonLayout::default());
        let ctx = PageContext::default();
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
        let zones = page.hit_zones(&ctx);
        assert_eq!(zones.len(), 2);
        assert_eq!(
            zones[0].action,
            HitAction::Custom(POWER_OFF_KEY.to_string())
        );
        assert_eq!(zones[1].action, HitAction::Back);
        let Some(PanelAction::Agent(req)) = page.on_custom(POWER_OFF_KEY, &ctx) else {
            panic!("power off must reach the agent");
        };
        assert_eq!(
            (req.method, req.path, req.body),
            ("POST", SHUTDOWN_PATH, None)
        );
        assert_eq!(page.on_custom("other", &ctx), None);
    }

    #[test]
    fn power_off_target_paints_the_destructive_fill() {
        let c = PowerPage::new(&ButtonLayout::default()).render(&PageContext::default(), &DARK);
        assert_eq!(c.pixel(POWER_BTN_X + 4, BTN_Y + 4), DARK.status_error);
        assert_eq!(c.pixel(CANCEL_BTN_X + 4, BTN_Y + 4), DARK.bg_secondary);
    }

    #[test]
    fn the_hint_names_the_mapped_buttons() {
        assert_eq!(
            power_hint(Some("B1_long"), Some("B4_short")),
            "Hold B1 again to power off, press B4 to cancel"
        );
        assert_eq!(
            power_hint(Some("B3_short"), Some("B2_long")),
            "Press B3 again to power off, hold B2 to cancel"
        );
        assert_eq!(power_hint(None, None), TOUCH_HINT);
        assert_eq!(
            power_hint(None, Some("B4_short")),
            "Tap Power off to confirm, press B4 to cancel"
        );
    }
}
