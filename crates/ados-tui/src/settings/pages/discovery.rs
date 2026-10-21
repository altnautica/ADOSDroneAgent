//! Settings page: discovery. The mDNS advertising switch and the reach URLs
//! the agent advertises (read-only, `y` copies the focused one).

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::Frame;

use super::kit;
use crate::app::{Core, FrameOut, Hit};
use crate::keymap::Command;
use crate::overlay::{OverlayResult, Ret};
use crate::poll::Sub;
use crate::settings::config_rows;
use crate::settings::page::{Bespoke, BespokeId};
use crate::settings::row;
use crate::settings::rows_view::{self, note, Item, RowsState};
use crate::worker::{Reply, Route};

const ROUTE: Route = Route::page("discovery");

const MDNS_HINT: &str = "Publish this node as a discoverable service on the LAN so Mission Control can find it by name. When off, reach it by IP.";
const LINKS_HINT: &str = "These are the reach URLs the agent advertises. Prefer an IP URL from a hosted ground station; a .local name resolves only from a device on this LAN. y copies the focused URL.";

#[derive(Default)]
pub struct Page {
    rows: RowsState,
}

impl Page {
    fn items(&self, core: &Core) -> Vec<Item> {
        let t = &core.theme;
        let mut items = vec![Item::Header("Discovery".into())];
        if let Some(n) = config_rows::status_note(core) {
            items.push(n);
        }
        items.push(Item::Row(config_rows::config_row(
            core,
            "discovery.mdns_enabled",
            "Advertise over mDNS",
            MDNS_HINT,
        )));
        items.push(Item::Header("Advertised links".into()));
        let urls = core
            .dash()
            .map(|d| d.access_urls.as_slice())
            .unwrap_or_default();
        if urls.is_empty() {
            items.push(note(core, "No reach URLs reported yet", false));
        }
        let w = urls
            .iter()
            .map(|u| u.label.chars().count())
            .max()
            .unwrap_or(0)
            .min(24);
        for (i, u) in urls.iter().enumerate() {
            items.push(Item::Entry {
                key: format!("url:{i}"),
                line: Line::from(vec![
                    Span::styled(format!("{:<w$}  ", u.label), t.dim()),
                    Span::styled(u.url.clone(), t.text()),
                ]),
            });
        }
        items
    }

    fn focused_url(&self, core: &Core, items: &[Item]) -> Option<String> {
        let i: usize = self
            .rows
            .focused_key(items)?
            .strip_prefix("url:")?
            .parse()
            .ok()?;
        core.dash()?.access_urls.get(i).map(|u| u.url.clone())
    }
}

impl Bespoke for Page {
    fn subs(&self, core: &Core) -> Vec<Sub> {
        config_rows::subs(core)
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        let items = self.items(core);
        if kit::nav(&mut self.rows, &items, cmd) {
            return true;
        }
        let row = self.rows.focused_row(&items).cloned();
        match cmd {
            Command::Left | Command::Right => {
                if let Some(row) = row {
                    kit::config_cycle(
                        core,
                        ROUTE,
                        BespokeId::Discovery.id(),
                        &row,
                        cmd == Command::Right,
                    );
                }
            }
            Command::Activate => {
                if let Some(row) = row {
                    kit::config_edit(core, ROUTE, BespokeId::Discovery.id(), &row);
                }
            }
            Command::Copy => {
                if let Some(url) = self.focused_url(core, &items) {
                    core.copy(&url);
                } else if let Some(row) = row {
                    core.copy(&row::display(&row.kind, row.value.as_ref(), false, true));
                }
            }
            _ => return false,
        }
        true
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        if ret.tag != kit::CONFIG_EDIT {
            return;
        }
        let items = self.items(core);
        if let Some(row) = kit::find_row(&items, ret.data.as_str().unwrap_or_default()).cloned() {
            kit::config_result(core, ROUTE, BespokeId::Discovery.id(), &row, &result);
        }
    }

    fn on_reply(&mut self, core: &mut Core, reply: Reply) {
        let items = self.items(core);
        kit::config_reply(core, &items, &reply);
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        let items = self.items(core);
        kit::click(&mut self.rows, &items, hit);
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        let items = self.items(core);
        let prefix = kit::prefix(BespokeId::Discovery.id());
        rows_view::render(f, area, core, &items, &mut self.rows, &prefix, true, out);
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        let items = self.items(core);
        match self.rows.focused_row(&items) {
            Some(r) => Some(r.help.clone()),
            None => Some(LINKS_HINT.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::poll::ResourceId;
    use crate::ui::tests::test_app;
    use serde_json::json;

    #[test]
    fn enter_on_the_mdns_toggle_flips_it_without_a_text_prompt() {
        let mut app = test_app();
        let core = &mut app.core;
        kit::put(
            core,
            ResourceId::ConfigSchema,
            json!({"properties": {"discovery": {"properties": {"mdns_enabled": {"type": "boolean", "default": true}}}}}),
        );
        kit::put(
            core,
            ResourceId::Config,
            json!({"discovery": {"mdns_enabled": true}}),
        );
        let mut page = Page::default();
        let items = page.items(core);
        page.rows.focus_key(&items, "discovery.mdns_enabled");
        let row = page
            .rows
            .focused_row(&items)
            .cloned()
            .expect("the toggle row");
        assert!(!row.read_only);
        assert_eq!(row.kind, crate::settings::schema::Kind::Bool);
        assert!(page.on_command(core, Command::Activate));
        assert!(
            core.overlays.is_empty(),
            "a toggle never opens an on/off prompt"
        );
    }
}
