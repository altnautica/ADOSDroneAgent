//! Row pages of the Settings tab (Config, Prefs, All settings): building the
//! rows, the `/` filter, ←/→ cycling, Enter editing, confirmation and the
//! write replies.

use serde_json::{json, Value};

use super::sidebar::Entry;
use super::SettingsTab;
use crate::app::{Core, TabId};
use crate::overlay::{Overlay, OverlayResult, Ret};
use crate::poll::ResourceId;
use crate::settings::all_settings;
use crate::settings::config_rows::{self, label_of};
use crate::settings::config_write;
use crate::settings::prefs_page;
use crate::settings::registry::{self, PageBody, NETWORK_TARGET};
use crate::settings::row::{cycle, display, edit_overlay, same, value_from_result, EditRow};
use crate::settings::rows_view::{note, Item};
use crate::settings::schema::{get, Kind};
use crate::toast::ToastKind;
use crate::worker::{Purpose, Reply, Route};

const TAG_EDIT: &str = "settings.edit";
const TAG_CONFIRM: &str = "settings.confirm";

/// A config write a key press asks for.
#[derive(Debug, Clone, PartialEq)]
pub struct Write {
    pub key: String,
    pub value: Value,
}

/// The write ←/→ (or Enter on a toggle) asks for: the next value of a bool
/// or short enum. `None` for read-only rows and kinds that do not cycle.
pub fn cycle_write(row: &EditRow, forward: bool) -> Option<Write> {
    if row.read_only {
        return None;
    }
    let value = cycle(&row.kind, row.value.as_ref(), forward)?;
    Some(Write {
        key: row.key.clone(),
        value,
    })
}

/// Keep rows whose label or key contains `filter` (case-insensitive), and the
/// headers above kept rows. Non-row lines are dropped while filtering.
pub fn filtered(items: Vec<Item>, filter: &str) -> Vec<Item> {
    let needle = filter.trim().to_lowercase();
    if needle.is_empty() {
        return items;
    }
    let mut out = Vec::new();
    let mut header: Option<Item> = None;
    for item in items {
        if matches!(item, Item::Header(_)) {
            header = Some(item);
            continue;
        }
        let keep = matches!(&item, Item::Row(r)
            if r.label.to_lowercase().contains(&needle) || r.key.to_lowercase().contains(&needle));
        if keep {
            if let Some(h) = header.take() {
                out.push(h);
            }
            out.push(item);
        }
    }
    out
}

/// The rows of a Config page.
fn config_items(core: &Core, rows: &[registry::Row]) -> Vec<Item> {
    let mut out = Vec::with_capacity(rows.len() + 4);
    if let Some(n) = config_rows::status_note(core) {
        out.push(n);
    }
    let mut section = "";
    for r in rows {
        if r.section != section {
            section = r.section;
            if !section.is_empty() {
                out.push(Item::Header(section.to_string()));
            }
        }
        let (label, help) = if r.key == "mavlink.serial_port" && network_transport(core) {
            NETWORK_TARGET
        } else {
            (r.label, r.help)
        };
        let mut row = config_rows::config_row(core, r.key, label, help);
        if !r.choices.is_empty() && !row.read_only {
            row.kind = Kind::Enum(r.choices.iter().map(|c| json!(c)).collect());
        }
        out.push(Item::Row(row));
    }
    out
}

/// Whether the MAVLink transport is a network one (the port row is a target).
fn network_transport(core: &Core) -> bool {
    core.value(&ResourceId::Config)
        .and_then(|c| get(c, "mavlink.source"))
        .and_then(Value::as_str)
        .is_some_and(|s| matches!(s, "udp" | "tcp"))
}

impl SettingsTab {
    /// The unfiltered lines of a row page; `None` for bespoke and plugin pages.
    pub(super) fn page_items(&self, core: &Core, entry: &Entry) -> Option<Vec<Item>> {
        Some(match entry.body {
            PageBody::Config(rows) => config_items(core, rows),
            PageBody::Prefs(p) => prefs_page::rows(p, &core.store.prefs)
                .into_iter()
                .map(Item::Row)
                .collect(),
            PageBody::AllSettings => match config_rows::leaves(core) {
                Some(leaves) => {
                    let mut items = Vec::new();
                    if let Some(n) = config_rows::status_note(core) {
                        items.push(n);
                    }
                    items.extend(all_settings::items(core, &leaves));
                    items
                }
                None => vec![config_rows::status_note(core)
                    .unwrap_or_else(|| note(core, "Reading the config schema…", false))],
            },
            PageBody::Bespoke(_) | PageBody::Plugin => return None,
        })
    }

    /// The lines of a row page after the `/` filter.
    pub(super) fn items(&self, core: &Core, entry: &Entry) -> Option<Vec<Item>> {
        self.page_items(core, entry)
            .map(|items| filtered(items, self.filter.value()))
    }

    fn flash_key(entry_id: &str, key: &str) -> String {
        format!("{entry_id}/{key}")
    }

    /// Apply `value` to `row` on a row page: preferences apply at once;
    /// config keys confirm first when the registry says so, then write.
    pub(super) fn apply(&mut self, core: &mut Core, entry: &Entry, row: &EditRow, value: Value) {
        if row.value.as_ref().is_some_and(|cur| same(cur, &value)) {
            return;
        }
        let flash = Self::flash_key(&entry.id, &row.key);
        if let PageBody::Prefs(_) = entry.body {
            if !prefs_page::set(&mut core.store.prefs, &row.key, &value) {
                core.toast(
                    ToastKind::Error,
                    format!("{}: not an accepted value", row.label),
                );
                core.flash(flash, false);
                return;
            }
            core.apply_prefs();
            core.save_prefs();
            if core.store.writable {
                core.toast(ToastKind::Success, format!("{} saved", row.label));
            } else {
                core.toast(
                    ToastKind::Warn,
                    format!(
                        "{} applied; preferences are not saved this session",
                        row.label
                    ),
                );
            }
            core.flash(flash, true);
            return;
        }
        // All settings rows are labelled by key; toasts and confirms use the
        // readable label.
        let label = if entry.body == PageBody::AllSettings {
            config_rows::config_row(core, &row.key, "", "").label
        } else {
            row.label.clone()
        };
        if registry::needs_confirm(&row.key, &value) {
            let shown = display(&row.kind, Some(&value), row.secret, false);
            let body = vec![
                format!("Set {label} to {shown}?"),
                confirm_reason(&row.key).to_string(),
            ];
            core.open(Overlay::confirm(
                Ret::new(Route::tab(TabId::Settings), TAG_CONFIRM).with(json!({
                    "page": entry.id, "key": row.key, "label": label, "value": value,
                })),
                format!("Change {label}"),
                body,
            ));
            return;
        }
        self.write(core, &entry.id, &row.key, &label, value);
    }

    fn write(&mut self, core: &mut Core, page: &str, key: &str, label: &str, value: Value) {
        let flash = Self::flash_key(page, key);
        self.pending.insert(flash.clone(), label.to_string());
        config_write::submit(core, Route::tab(TabId::Settings), key, value, &flash);
    }

    /// ←/→ on the focused row.
    pub(super) fn cycle_focused(&mut self, core: &mut Core, entry: &Entry, forward: bool) {
        let Some(items) = self.items(core, entry) else {
            return;
        };
        let Some(row) = self.state(&entry.id).focused_row(&items).cloned() else {
            return;
        };
        if let Some(w) = cycle_write(&row, forward) {
            self.apply(core, entry, &row, w.value);
        }
    }

    /// Enter on the focused row: toggles flip, everything else opens its editor.
    pub(super) fn edit_focused(&mut self, core: &mut Core, entry: &Entry) {
        let Some(items) = self.items(core, entry) else {
            return;
        };
        let Some(row) = self.state(&entry.id).focused_row(&items).cloned() else {
            return;
        };
        if row.kind == Kind::Bool && !row.read_only {
            self.cycle_focused(core, entry, true);
            return;
        }
        let ret = Ret::new(Route::tab(TabId::Settings), TAG_EDIT).with(json!({
            "page": entry.id, "key": row.key,
        }));
        match edit_overlay(&row, ret) {
            Some(overlay) => core.open(overlay),
            None => core.toast(ToastKind::Info, format!("{} is read-only here", row.label)),
        }
    }

    /// An overlay a row page opened finished.
    pub(super) fn row_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        let s = |k: &str| {
            ret.data
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let (page, key) = (s("page"), s("key"));
        match ret.tag {
            TAG_CONFIRM if result == OverlayResult::Confirmed => {
                let value = ret.data.get("value").cloned().unwrap_or(Value::Null);
                self.write(core, &page, &key, &s("label"), value);
            }
            TAG_EDIT => {
                let Some(entry) = self.entry(core, &page) else {
                    return;
                };
                let Some(items) = self.page_items(core, &entry) else {
                    return;
                };
                let Some(row) = items.iter().find_map(|i| match i {
                    Item::Row(r) if r.key == key => Some(r.clone()),
                    _ => None,
                }) else {
                    return;
                };
                match value_from_result(&row.kind, &result) {
                    Some(Ok(value)) => self.apply(core, &entry, &row, value),
                    Some(Err(why)) => {
                        core.toast(ToastKind::Error, format!("{}: {why}", row.label));
                        core.flash(Self::flash_key(&page, &key), false);
                    }
                    None => {}
                }
            }
            _ => {}
        }
    }

    /// A config write a row page sent finished.
    pub(super) fn row_reply(&mut self, core: &mut Core, reply: Reply) {
        if let Purpose::ConfigSet { key, row } = &reply.purpose {
            let label = self
                .pending
                .remove(row)
                .unwrap_or_else(|| label_of(key, None));
            config_write::on_reply(core, &label, row, &reply.result);
        }
    }

    /// The text `y` copies from the focused row. Secrets copy only while
    /// revealed.
    pub(super) fn copy_focused(&self, core: &mut Core, entry: &Entry) {
        let Some(items) = self.items(core, entry) else {
            return;
        };
        let st = self.state(&entry.id);
        let Some(row) = st.focused_row(&items) else {
            return;
        };
        if row.secret && !st.revealing(core.now) {
            core.toast(
                ToastKind::Warn,
                "Press v to reveal the secret before copying it",
            );
            return;
        }
        let text = display(&row.kind, row.value.as_ref(), row.secret, true);
        core.copy(&text);
    }
}

/// Why a confirmed key asks first.
fn confirm_reason(key: &str) -> &'static str {
    match key {
        "mavlink.ws_proxy_enforce_auth" => {
            "MAVLink WebSocket clients without a valid key will be refused once this is on."
        }
        k if k.starts_with("mavlink.") => {
            "The flight-controller link reconnects with the new setting; a wrong value drops the FC link."
        }
        _ => "The agent applies the change immediately.",
    }
}
