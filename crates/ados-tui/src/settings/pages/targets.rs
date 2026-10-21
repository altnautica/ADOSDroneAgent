//! Settings page: THIS TERMINAL › Agents. The saved agent targets: connect,
//! add, edit and delete. The list lives in `tui.json`; no agent call.

use std::collections::HashMap;

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::Frame;
use serde_json::{json, Value};

use super::kit;
use crate::app::{Core, FrameOut, Hit};
use crate::args::{load_pairing_key, split_host_port, Target, DEFAULT_PORT};
use crate::keymap::{Command, PageKeys};
use crate::overlay::{Overlay, OverlayResult, Ret};
use crate::prefs::SavedTarget;
use crate::settings::page::{Bespoke, BespokeId};
use crate::settings::rows_view::{self, Item, RowsState};
use crate::toast::ToastKind;
use crate::worker::Route;

const LOCAL_KEY: &str = "local";

fn route() -> Route {
    Route::page(BespokeId::Targets.id())
}

#[derive(Default)]
pub struct Page {
    rows: RowsState,
    /// Keys typed for a target but not remembered: used for a connect in this
    /// session only, never written anywhere.
    session_keys: HashMap<String, String>,
}

fn entry_key(i: usize) -> String {
    format!("t{i}")
}

fn index_of(key: &str) -> Option<usize> {
    key.strip_prefix('t').and_then(|n| n.parse().ok())
}

fn validate_name(s: &str) -> Result<(), String> {
    let s = s.trim();
    if s.is_empty() {
        Err("Enter a name".into())
    } else if s.eq_ignore_ascii_case(LOCAL_KEY) {
        Err("\"local\" is reserved for this machine".into())
    } else {
        Ok(())
    }
}

fn validate_host(s: &str) -> Result<(), String> {
    let s = s.trim();
    if s.is_empty() || s.contains(char::is_whitespace) || s.contains('/') {
        return Err("Enter a host name or address, e.g. 192.168.1.50".into());
    }
    split_host_port(s).map(|_| ())
}

fn validate_port(s: &str) -> Result<(), String> {
    match s.trim().parse::<u16>() {
        Ok(p) if p > 0 => Ok(()),
        _ => Err("Enter a port from 1 to 65535".into()),
    }
}

fn str_of(data: &Value, k: &str) -> String {
    data.get(k).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn edit_index(data: &Value) -> Option<usize> {
    data.get("edit").and_then(Value::as_u64).map(|i| i as usize)
}

impl Page {
    fn items(&self, core: &Core) -> Vec<Item> {
        let t = &core.theme;
        let current = |is: bool| {
            if is {
                Span::styled("● ", t.accented())
            } else {
                Span::raw("  ")
            }
        };
        let mut items = vec![Item::Header("Agents".into())];
        items.push(Item::Entry {
            key: LOCAL_KEY.into(),
            line: Line::from(vec![
                current(!core.target.remote),
                Span::styled(format!("{:<20}", "local"), t.text()),
                Span::styled(format!("{:<28}", format!("localhost:{DEFAULT_PORT}")), t.dim()),
                Span::styled("key from the pairing file", t.dim()),
            ]),
        });
        for (i, s) in core.store.prefs.targets.iter().enumerate() {
            let is = core.target.remote && core.target.base == s.base();
            let key = if s.api_key.is_some() {
                "key stored: yes"
            } else if self.session_keys.contains_key(&s.name) {
                "key stored: this session"
            } else {
                "key stored: no"
            };
            items.push(Item::Entry {
                key: entry_key(i),
                line: Line::from(vec![
                    current(is),
                    Span::styled(format!("{:<20}", s.name), t.text()),
                    Span::styled(format!("{:<28}", format!("{}:{}", s.host, s.port)), t.dim()),
                    Span::styled(key, t.dim()),
                ]),
            });
        }
        if core.store.prefs.targets.is_empty() {
            items.push(rows_view::note(core, "No saved agents. Press n to add one.", false));
        }
        if !core.store.writable {
            items.push(rows_view::note(
                core,
                "tui.json is not writable this session: changes last until you quit.",
                true,
            ));
        }
        items
    }

    fn focused_index(&self, core: &Core) -> Option<usize> {
        let items = self.items(core);
        self.rows.focused_key(&items).and_then(index_of)
    }

    fn connect(&mut self, core: &mut Core) {
        let items = self.items(core);
        match self.rows.focused_key(&items) {
            Some(LOCAL_KEY) => {
                if core.target.remote {
                    core.retarget = Some((Target::local(), load_pairing_key()));
                } else {
                    core.toast(ToastKind::Info, "Already connected to local");
                }
            }
            Some(key) => {
                let Some(s) = index_of(key).and_then(|i| core.store.prefs.targets.get(i)).cloned() else {
                    return;
                };
                if core.target.remote && core.target.base == s.base() {
                    core.toast(ToastKind::Info, format!("Already connected to {}", s.name));
                    return;
                }
                let key = s.api_key.clone().or_else(|| self.session_keys.get(&s.name).cloned());
                core.retarget = Some((Target::from_saved(&s), key));
            }
            None => {}
        }
    }

    fn ask_name(core: &mut Core, data: Value, error: Option<String>) {
        let mut s = Overlay::input(
            Ret::new(route(), "name").with(data.clone()),
            if edit_index(&data).is_some() { "Edit agent" } else { "Add agent" },
            "Name",
            &str_of(&data, "name"),
        );
        s.validate = Some(validate_name);
        s.error = error;
        core.open(Overlay::Input(s));
    }

    /// Save the drafted target. `key` is what was typed (`None` = left
    /// empty); `remember` stores it in `tui.json`.
    fn finish(&mut self, core: &mut Core, data: &Value, key: Option<String>, remember: bool) {
        let name = str_of(data, "name");
        let edit = edit_index(data);
        let kept = edit
            .and_then(|i| core.store.prefs.targets.get(i))
            .and_then(|t| t.api_key.clone());
        let api_key = match (&key, remember) {
            (Some(k), true) => Some(k.clone()),
            (Some(_), false) => None,
            (None, _) => kept,
        };
        if let Some(old) = edit.and_then(|i| core.store.prefs.targets.get(i)) {
            let old_name = old.name.clone();
            if let Some(k) = self.session_keys.remove(&old_name) {
                self.session_keys.insert(name.clone(), k);
            }
        }
        match (key, remember) {
            (Some(k), false) => {
                self.session_keys.insert(name.clone(), k);
            }
            (Some(_), true) => {
                self.session_keys.remove(&name);
            }
            (None, _) => {}
        }
        let target = SavedTarget {
            name: name.clone(),
            host: str_of(data, "host"),
            port: data.get("port").and_then(Value::as_u64).unwrap_or(u64::from(DEFAULT_PORT)) as u16,
            api_key,
        };
        let pos = match edit {
            Some(i) if i < core.store.prefs.targets.len() => {
                core.store.prefs.targets[i] = target;
                i
            }
            _ => {
                core.store.prefs.targets.push(target);
                core.store.prefs.targets.len() - 1
            }
        };
        core.save_prefs();
        core.toast(ToastKind::Success, format!("{name} saved"));
        let items = self.items(core);
        self.rows.focus_key(&items, &entry_key(pos));
    }
}

impl Bespoke for Page {
    fn keys(&self) -> Option<PageKeys> {
        Some(PageKeys::Targets)
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        let items = self.items(core);
        if kit::nav(&mut self.rows, &items, cmd) {
            return true;
        }
        match cmd {
            Command::Activate => self.connect(core),
            Command::Add => Self::ask_name(core, json!({ "edit": null }), None),
            Command::Edit => match self.focused_index(core).and_then(|i| core.store.prefs.targets.get(i).map(|t| (i, t.clone()))) {
                Some((i, t)) => Self::ask_name(
                    core,
                    json!({ "edit": i, "name": t.name, "host": t.host, "port": t.port, "has_key": t.api_key.is_some() }),
                    None,
                ),
                None => core.toast(ToastKind::Info, "Select a saved agent to edit"),
            },
            Command::Delete => match self.focused_index(core).and_then(|i| core.store.prefs.targets.get(i).map(|t| (i, t.name.clone()))) {
                Some((i, name)) => core.open(Overlay::confirm(
                    Ret::new(route(), "delete").with(json!({ "index": i, "name": name })),
                    "Delete agent?",
                    vec![format!("Remove {name} from the saved agents (and any stored key)?")],
                )),
                None => core.toast(ToastKind::Info, "Select a saved agent to delete"),
            },
            Command::Copy => match self.rows.focused_key(&items) {
                Some(LOCAL_KEY) => core.copy(&format!("localhost:{DEFAULT_PORT}")),
                Some(k) => {
                    if let Some(t) = index_of(k).and_then(|i| core.store.prefs.targets.get(i)) {
                        let text = format!("{}:{}", t.host, t.port);
                        core.copy(&text);
                    }
                }
                None => return false,
            },
            _ => return false,
        }
        true
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        let mut data = ret.data.clone();
        match (ret.tag, result) {
            ("name", OverlayResult::Text(text)) => {
                let name = text.trim().to_string();
                let edit = edit_index(&data);
                let dup = core
                    .store
                    .prefs
                    .targets
                    .iter()
                    .enumerate()
                    .any(|(j, t)| t.name == name && Some(j) != edit);
                data["name"] = json!(name);
                if dup {
                    Self::ask_name(core, data, Some(format!("An agent named {name} is already saved")));
                    return;
                }
                let mut s = Overlay::input(Ret::new(route(), "host").with(data.clone()), "Agent host", "Host or address", &str_of(&data, "host"));
                s.validate = Some(validate_host);
                s.hint = Some("e.g. 192.168.1.50 or drone.local".into());
                core.open(Overlay::Input(s));
            }
            ("host", OverlayResult::Text(text)) => {
                let Ok((host, port)) = split_host_port(&text) else { return };
                data["host"] = json!(host);
                let typed_port = text
                    .trim()
                    .rsplit_once(':')
                    .is_some_and(|(h, p)| p.parse::<u16>().is_ok() && (!h.contains(':') || h.ends_with(']')));
                let port = if typed_port {
                    port
                } else {
                    data.get("port").and_then(Value::as_u64).map(|p| p as u16).unwrap_or(DEFAULT_PORT)
                };
                let mut s = Overlay::input(Ret::new(route(), "port").with(data), "Agent port", "Port", &port.to_string());
                s.validate = Some(validate_port);
                core.open(Overlay::Input(s));
            }
            ("port", OverlayResult::Text(text)) => {
                let Ok(port) = text.trim().parse::<u16>() else { return };
                data["port"] = json!(port);
                let has_key = data.get("has_key").and_then(Value::as_bool).unwrap_or(false);
                let mut s = Overlay::input(Ret::new(route(), "key").with(data), "Agent API key", "API key (optional)", "");
                s.masked = true;
                s.hint = Some(if has_key {
                    "Leave empty to keep the stored key".into()
                } else {
                    "Leave empty to be asked when connecting".into()
                });
                core.open(Overlay::Input(s));
            }
            ("key", OverlayResult::Text(key)) => {
                let key = key.trim().to_string();
                if key.is_empty() {
                    self.finish(core, &data, None, false);
                    return;
                }
                let name = str_of(&data, "name");
                core.open(Overlay::confirm(
                    Ret::new(route(), "remember").with(json!({ "draft": data, "key": key })),
                    "Remember the key?",
                    vec![
                        format!("Store the key for {name} in tui.json (an owner-only file)?"),
                        "No keeps it for this session only.".into(),
                    ],
                ));
            }
            ("remember", r @ (OverlayResult::Confirmed | OverlayResult::Cancelled)) => {
                let draft = data.get("draft").cloned().unwrap_or(Value::Null);
                let key = str_of(&data, "key");
                self.finish(core, &draft, Some(key), r == OverlayResult::Confirmed);
            }
            ("delete", OverlayResult::Confirmed) => {
                let i = data.get("index").and_then(Value::as_u64).unwrap_or(u64::MAX) as usize;
                let name = str_of(&data, "name");
                if core.store.prefs.targets.get(i).is_some_and(|t| t.name == name) {
                    core.store.prefs.targets.remove(i);
                    self.session_keys.remove(&name);
                    core.save_prefs();
                    core.toast(ToastKind::Success, format!("{name} deleted"));
                }
            }
            _ => {}
        }
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        let items = self.items(core);
        kit::click(&mut self.rows, &items, hit);
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        let items = self.items(core);
        rows_view::render(f, area, core, &items, &mut self.rows, "agents:", true, out);
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        let items = self.items(core);
        match self.rows.focused_key(&items)? {
            LOCAL_KEY => Some("The agent on this machine; Enter connects to it".into()),
            k => {
                let t = index_of(k).and_then(|i| core.store.prefs.targets.get(i))?;
                Some(format!("Enter connects to {}", t.base()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prefs::PrefsStore;
    use crate::ui::tests::test_app;

    fn take_ret(core: &mut Core) -> Ret {
        match core.overlays.pop() {
            Some(Overlay::Input(s)) => s.ret,
            Some(Overlay::Confirm(c)) => c.ret,
            other => panic!("expected a prompt, got {other:?}"),
        }
    }

    #[test]
    fn adding_and_deleting_a_target_persists_to_prefs() {
        let dir = std::env::temp_dir().join(format!("ados-tui-targets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("tui.json");
        let mut app = test_app();
        let core = &mut app.core;
        core.store = PrefsStore::load(Some(path.clone()));
        let mut page = Page::default();

        assert!(page.on_command(core, Command::Add));
        let ret = take_ret(core);
        page.on_overlay(core, ret, OverlayResult::Text("bench".into()));
        let ret = take_ret(core);
        page.on_overlay(core, ret, OverlayResult::Text("192.168.1.50".into()));
        let ret = take_ret(core);
        page.on_overlay(core, ret, OverlayResult::Text("8080".into()));
        let ret = take_ret(core);
        page.on_overlay(core, ret, OverlayResult::Text("secret-key".into()));
        let ret = take_ret(core);
        assert_eq!(ret.tag, "remember");
        page.on_overlay(core, ret, OverlayResult::Confirmed);

        let saved = PrefsStore::load(Some(path.clone())).prefs.targets;
        assert_eq!(
            saved,
            vec![SavedTarget {
                name: "bench".into(),
                host: "192.168.1.50".into(),
                port: 8080,
                api_key: Some("secret-key".into()),
            }]
        );

        // The new entry has focus; delete it.
        assert!(page.on_command(core, Command::Delete));
        let ret = take_ret(core);
        page.on_overlay(core, ret, OverlayResult::Confirmed);
        assert!(PrefsStore::load(Some(path)).prefs.targets.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_key_that_is_not_remembered_is_never_saved() {
        let mut app = test_app();
        let core = &mut app.core;
        let mut page = Page::default();
        let data = json!({ "edit": null, "name": "field", "host": "192.168.1.60", "port": 8080 });
        page.on_overlay(core, Ret::new(route(), "key").with(data), OverlayResult::Text("k".into()));
        let ret = take_ret(core);
        page.on_overlay(core, ret, OverlayResult::Cancelled);
        assert_eq!(core.store.prefs.targets[0].api_key, None);
        assert_eq!(page.session_keys.get("field").map(String::as_str), Some("k"));
    }

    #[test]
    fn names_hosts_and_ports_are_validated() {
        assert!(validate_name(" ").is_err());
        assert!(validate_name("local").is_err());
        assert!(validate_host("http://x/y").is_err());
        assert!(validate_host("192.168.1.50").is_ok());
        assert!(validate_port("0").is_err());
        assert!(validate_port("8080").is_ok());
    }
}
