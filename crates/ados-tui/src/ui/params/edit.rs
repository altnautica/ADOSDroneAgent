//! Params tab actions: editing, staging, favourites, file import/export, and
//! the batch write.

use std::time::{SystemTime, UNIX_EPOCH};

use ados_protocol::rest::RestError;
use serde_json::{json, Value};

use super::{Batch, ParamsTab, ROUTE};
use crate::app::{error_text, Core};
use crate::overlay::{CheckItem, ChecklistState, Overlay, OverlayResult, Ret};
use crate::params::file_io;
use crate::params::metadata::{format_labeled, plain};
use crate::params::store::StageOutcome;
use crate::toast::ToastKind;
use crate::worker::Request;

/// The refusal for a write without a fresh disarmed reading.
pub const WRITE_REFUSAL: &str = "Disarm and wait for a fresh FC reading";

/// Confirm-dialog rows shown before the list is cut.
const CONFIRM_ROWS: usize = 16;

fn finite(text: &str) -> Result<(), String> {
    match text.trim().parse::<f64>() {
        Ok(v) if v.is_finite() => Ok(()),
        _ => Err("Enter a finite number".to_string()),
    }
}

fn name_of(ret: &Ret) -> Option<String> {
    ret.data
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
}

impl ParamsTab {
    /// The value an edit starts from: the staged one, else the live one.
    fn current(&self, name: &str) -> Option<f64> {
        self.store.staged_value(name).or(self.store.value(name))
    }

    pub(super) fn edit(&mut self, core: &mut Core) {
        let Some(name) = self.focused() else { return };
        if self.store.read_only(&name) {
            core.toast(ToastKind::Warn, format!("{name} is read-only"));
            return;
        }
        let Some(cur) = self.current(&name) else {
            return;
        };
        let meta = self.store.meta(&name).cloned().unwrap_or_default();
        let ret = Ret::new(ROUTE, "edit").with(json!({ "name": name }));
        if let Some(values) = meta.enum_values() {
            let items: Vec<String> = values
                .iter()
                .map(|(code, label)| format!("{} {label}", plain(*code)))
                .collect();
            let selected = values
                .iter()
                .position(|(code, _)| crate::params::same(*code, cur))
                .unwrap_or(0);
            core.open(Overlay::picker(
                ret.with(json!({ "name": name, "kind": "enum" })),
                name,
                items,
                selected,
            ));
        } else if let Some(bits) = meta.bits() {
            let raw = cur.max(0.0) as u64;
            let items = bits
                .iter()
                .map(|(bit, label)| CheckItem {
                    label: format!("{bit:>2} {label}"),
                    checked: raw & (1u64 << bit) != 0,
                    locked: false,
                    bit: Some(1u64 << bit),
                })
                .collect();
            core.open(Overlay::Checklist(ChecklistState {
                title: name.clone(),
                items,
                selected: 0,
                footer: None,
                ret: ret.with(json!({ "name": name, "kind": "bits" })),
            }));
        } else {
            let mut input = Overlay::input(
                ret.with(json!({ "name": name, "kind": "number" })),
                name.clone(),
                "New value",
                &plain(cur),
            );
            input.validate = Some(finite);
            let mut hint = Vec::new();
            if let Some(r) = meta.range {
                hint.push(format!("range {} .. {}", plain(r.min), plain(r.max)));
            }
            if let Some(step) = meta.increment {
                hint.push(format!("step {}", plain(step)));
            }
            if let Some(units) = meta.units.as_deref().filter(|u| !u.is_empty()) {
                hint.push(units.to_string());
            }
            if !hint.is_empty() {
                input.hint = Some(hint.join(" · "));
            }
            core.open(Overlay::Input(input));
        }
    }

    fn stage(&mut self, core: &mut Core, name: &str, value: f64) {
        match self.store.stage(name, value) {
            StageOutcome::Staged => {
                if let Some(r) = self
                    .store
                    .meta(name)
                    .filter(|m| m.out_of_range(value))
                    .and_then(|m| m.range)
                {
                    core.toast(
                        ToastKind::Warn,
                        format!(
                            "{name}: {} is outside {} .. {}; staged anyway",
                            plain(value),
                            plain(r.min),
                            plain(r.max)
                        ),
                    );
                }
            }
            StageOutcome::Unchanged => core.toast(ToastKind::Info, format!("{name} unchanged")),
            StageOutcome::ReadOnly => core.toast(ToastKind::Warn, format!("{name} is read-only")),
        }
        self.clamp();
    }

    pub(super) fn unstage(&mut self, core: &mut Core) {
        if let Some(name) = self.focused() {
            if self.store.unstage(&name) {
                core.toast(ToastKind::Info, format!("{name} unstaged"));
            }
            self.clamp();
        }
    }

    pub(super) fn discard_all(&mut self, core: &mut Core) {
        let n = self.staged_count();
        if n == 0 {
            core.toast(ToastKind::Info, "Nothing staged");
            return;
        }
        core.open(Overlay::confirm(
            Ret::new(ROUTE, "discard"),
            "Discard staged values?",
            vec![format!("{n} staged change(s) will be dropped.")],
        ));
    }

    pub(super) fn favourite(&mut self, core: &mut Core) {
        let (Some(name), Some(key)) = (self.focused(), self.fav_key) else {
            return;
        };
        let now = self.store.toggle_favourite(&name);
        let favs = self.store.favourites();
        if favs.is_empty() {
            core.store.prefs.favorite_params.remove(key);
        } else {
            core.store
                .prefs
                .favorite_params
                .insert(key.to_string(), favs);
        }
        core.save_prefs();
        let word = if now { "added to" } else { "removed from" };
        core.toast(ToastKind::Info, format!("{name} {word} favourites"));
        self.clamp();
    }

    pub(super) fn export(&mut self, core: &mut Core) {
        if self.store.live().is_empty() {
            core.toast(ToastKind::Warn, "No parameters loaded");
            return;
        }
        let device = core
            .dash()
            .map(|d| d.device_name.clone())
            .unwrap_or_default();
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let file = file_io::export_name(&device, secs);
        let text = file_io::export_text(&self.store);
        match std::fs::write(&file, text) {
            Ok(()) => core.toast(
                ToastKind::Success,
                format!(
                    "Exported {} parameters to ./{file}",
                    self.store.live().len()
                ),
            ),
            Err(e) => core.toast(ToastKind::Error, format!("Export failed: {e}")),
        }
    }

    pub(super) fn import_prompt(&mut self, core: &mut Core) {
        if self.store.live().is_empty() {
            core.toast(ToastKind::Warn, "No parameters loaded");
            return;
        }
        let mut input = Overlay::input(
            Ret::new(ROUTE, "import"),
            "Import parameters",
            "Path to a .param file",
            "",
        );
        let place = if core.target.remote {
            " (a path on this machine)"
        } else {
            ""
        };
        input.hint = Some(format!(
            "NAME,VALUE or NAME VALUE per line, # comments{place}"
        ));
        core.open(Overlay::Input(input));
    }

    fn import_file(&mut self, core: &mut Core, path: &str) {
        let path = path.trim();
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let counts = file_io::import(&mut self.store, &text);
                self.clamp();
                core.toast(ToastKind::Info, counts.summary());
            }
            Err(e) => core.toast(ToastKind::Error, format!("Import failed: {path}: {e}")),
        }
    }

    pub(super) fn write(&mut self, core: &mut Core) {
        if self.batch.is_some() {
            core.toast(ToastKind::Warn, "A parameter write is already running");
            return;
        }
        if self.staged_count() == 0 {
            core.toast(ToastKind::Info, "Nothing staged");
            return;
        }
        if core.require_disarmed().is_err() {
            core.toast(ToastKind::Error, WRITE_REFUSAL);
            return;
        }
        let mut body = vec![format!(
            "Write {} parameter(s) to the flight controller?",
            self.staged_count()
        )];
        for (name, staged) in self.store.staged().iter().take(CONFIRM_ROWS) {
            let meta = self.store.meta(name);
            let old = staged
                .base
                .or(self.store.value(name))
                .map(|v| format_labeled(v, meta))
                .unwrap_or_else(|| "?".into());
            let new = format_labeled(staged.value, meta);
            let warn = if meta.is_some_and(|m| m.out_of_range(staged.value)) {
                format!("  {} out of range", self.out_of_range_glyph(core))
            } else {
                String::new()
            };
            body.push(format!("{name}  {old} → {new}{warn}"));
        }
        if self.staged_count() > CONFIRM_ROWS {
            body.push(format!("… and {} more", self.staged_count() - CONFIRM_ROWS));
        }
        core.open(Overlay::confirm(
            Ret::new(ROUTE, "write"),
            "Write parameters",
            body,
        ));
    }

    pub(super) fn submit_batch(&mut self, core: &mut Core) {
        if core.require_disarmed().is_err() {
            core.toast(ToastKind::Error, WRITE_REFUSAL);
            return;
        }
        let items: Vec<(String, f64)> = self
            .store
            .staged()
            .iter()
            .map(|(n, s)| (n.clone(), s.value))
            .collect();
        if items.is_empty() {
            return;
        }
        self.batch = Some(Batch {
            total: items.len(),
            ..Batch::default()
        });
        core.submit(Request::param_batch(ROUTE, items));
    }

    pub(super) fn batch_item(
        &mut self,
        core: &mut Core,
        name: String,
        value: f64,
        result: Result<Value, RestError>,
    ) {
        let row = format!("param:{name}");
        let reboot = self.store.meta(&name).is_some_and(|m| m.reboot_required);
        let Some(batch) = self.batch.as_mut() else {
            return;
        };
        batch.done += 1;
        match result {
            Ok(body) if body.get("ack").and_then(Value::as_bool) == Some(true) => {
                batch.acked += 1;
                batch.reboot |= reboot;
                let confirmed = body
                    .get("cached_value")
                    .and_then(Value::as_f64)
                    .unwrap_or(value);
                self.store.unstage(&name);
                self.store.set_one(&name, confirmed);
                self.clamp();
                core.flash(row, true);
            }
            Ok(_) => core.toast(ToastKind::Warn, format!("{name}: sent, not confirmed")),
            Err(e) => {
                core.toast(ToastKind::Error, format!("{name}: {}", error_text(&e)));
                core.flash(row, false);
            }
        }
    }

    pub(super) fn batch_done(&mut self, core: &mut Core) {
        let Some(b) = self.batch.take() else { return };
        let mut text = format!("Wrote {}/{}", b.acked, b.total);
        if b.reboot {
            text.push_str("; reboot the flight controller to apply");
        }
        let kind = if b.acked == b.total {
            ToastKind::Success
        } else {
            ToastKind::Warn
        };
        core.toast(kind, text);
        core.fetch_now(crate::poll::ResourceId::Params);
    }

    pub(super) fn overlay_result(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        match (ret.tag, result) {
            ("edit", result) => {
                let Some(name) = name_of(&ret) else { return };
                let meta = self.store.meta(&name).cloned().unwrap_or_default();
                match result {
                    OverlayResult::Picked(i) => {
                        if let Some((code, _)) = meta.enum_values().and_then(|v| v.get(i)) {
                            self.stage(core, &name, *code);
                        }
                    }
                    OverlayResult::Checked(checked) => {
                        let Some(bits) = meta.bits() else { return };
                        let cur = self.current(&name).unwrap_or(0.0).max(0.0) as u64;
                        let known: u64 = bits.iter().map(|(b, _)| 1u64 << b).fold(0, |a, b| a | b);
                        let set: u64 = bits
                            .iter()
                            .zip(checked)
                            .filter(|(_, on)| *on)
                            .map(|((b, _), _)| 1u64 << b)
                            .fold(0, |a, b| a | b);
                        // Bits the catalog does not name are kept as they were.
                        self.stage(core, &name, ((cur & !known) | set) as f64);
                    }
                    OverlayResult::Text(text) => match text.trim().parse::<f64>() {
                        Ok(v) if v.is_finite() => self.stage(core, &name, v),
                        _ => core.toast(ToastKind::Error, "Enter a finite number"),
                    },
                    _ => {}
                }
            }
            ("discard", OverlayResult::Confirmed) => {
                let n = self.store.discard_all();
                self.clamp();
                core.toast(ToastKind::Info, format!("Discarded {n} staged change(s)"));
            }
            ("import", OverlayResult::Text(path)) => self.import_file(core, &path),
            ("write", OverlayResult::Confirmed) => self.submit_batch(core),
            _ => {}
        }
    }

    /// The out-of-range marker for the symbol preset.
    pub(super) fn out_of_range_glyph(&self, core: &Core) -> &'static str {
        if core.sym.name == "ascii" {
            "!"
        } else {
            "⚠"
        }
    }

    /// The drift marker for the symbol preset.
    pub(super) fn drift_glyph(&self, core: &Core) -> &'static str {
        if core.sym.name == "ascii" {
            "!="
        } else {
            "≠"
        }
    }
}
