//! The install flow shared by catalog entries and typed URLs:
//! `parse_from_url` preview → permission checklist (required locked on) →
//! `install_from_url` with the approved set → list refresh. A required
//! permission the agent did not grant disables the plugin again, so it never
//! runs half-granted.

use ados_protocol::rest::RestError;
use serde_json::{json, Value};

use super::state::{self, Perm};
use crate::api_path::seg;
use crate::app::Core;
use crate::overlay::{ChecklistState, Overlay, OverlayResult, Ret};
use crate::poll::ResourceId;
use crate::toast::ToastKind;
use crate::worker::{Purpose, Request, Route, INSTALL_TIMEOUT};

/// The overlay tag of the permission checklist.
pub const TAG_PERMS: &str = "ext.perms";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Downloading and verifying the archive for the preview.
    Parsing,
    /// The permission checklist is open.
    Choosing,
    /// Downloading, verifying and installing.
    Installing,
}

#[derive(Debug, Clone)]
struct Flow {
    url: String,
    sha: Option<String>,
    from_catalog: bool,
    name: String,
    stage: Stage,
    perms: Vec<Perm>,
}

/// At most one install runs at a time.
#[derive(Debug, Default)]
pub struct Installer {
    flow: Option<Flow>,
}

impl Installer {
    pub fn busy(&self) -> bool {
        self.flow.is_some()
    }

    /// The spinner line while a request runs.
    pub fn status(&self) -> Option<String> {
        let f = self.flow.as_ref()?;
        match f.stage {
            Stage::Parsing => Some(format!("Fetching and verifying {}…", f.name)),
            Stage::Choosing => None,
            Stage::Installing => Some(format!("Installing {}… (up to 3 min)", f.name)),
        }
    }

    /// Start with the preview. Armed-guarded; one install at a time.
    pub fn start(&mut self, core: &mut Core, route: Route, url: String, sha: Option<String>, from_catalog: bool, name: String) {
        if self.busy() {
            core.toast(ToastKind::Warn, "An install is already in progress");
            return;
        }
        if !core.guard_disarmed() {
            return;
        }
        let sha = sha.map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty());
        core.submit(
            Request::new("POST", "/api/plugins/parse_from_url", route, Purpose::PluginParse)
                .body(state::parse_body(&url, sha.as_deref()))
                .timeout(INSTALL_TIMEOUT),
        );
        self.flow = Some(Flow {
            url,
            sha,
            from_catalog,
            name,
            stage: Stage::Parsing,
            perms: Vec::new(),
        });
    }

    /// The preview arrived: show the permission checklist.
    pub fn on_parse(&mut self, core: &mut Core, route: Route, result: &Result<Value, RestError>) {
        let Some(flow) = self.flow.as_mut() else { return };
        let summary = match result {
            Ok(v) => v,
            Err(e) => {
                core.toast_error("Install preview failed", e);
                self.flow = None;
                return;
            }
        };
        flow.perms = state::perms(summary);
        // Pin the install to the archive the preview verified.
        if flow.sha.is_none() {
            flow.sha = summary.get("archive_sha256").and_then(Value::as_str).map(str::to_string);
        }
        let str_at = |k: &str| summary.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let name = str_at("name");
        if !name.is_empty() {
            flow.name = name;
        }
        let signer = match (summary.get("signed").and_then(Value::as_bool), str_at("signer_id")) {
            (Some(true), s) if !s.is_empty() => format!("signed by {s}"),
            (Some(true), _) => "signed".to_string(),
            _ => "unsigned".to_string(),
        };
        let footer = format!(
            "Risk {} · {signer} · Space toggles an optional permission · Enter installs · Esc cancels",
            str_at("risk")
        );
        flow.stage = Stage::Choosing;
        core.open(Overlay::Checklist(ChecklistState {
            title: format!("Install {} {}?", flow.name, str_at("version")),
            items: state::checklist_items(&flow.perms),
            selected: 0,
            footer: Some(footer),
            ret: Ret::new(route, TAG_PERMS),
        }));
    }

    /// The checklist closed: install with the approved set, or stop.
    pub fn on_perms(&mut self, core: &mut Core, route: Route, result: OverlayResult) {
        let Some(flow) = self.flow.as_mut() else { return };
        let OverlayResult::Checked(checked) = result else {
            self.flow = None;
            return;
        };
        if !core.guard_disarmed() {
            self.flow = None;
            return;
        }
        let requested = state::requested(&flow.perms, &checked);
        let body = state::install_body(&flow.url, flow.sha.as_deref(), requested, flow.from_catalog);
        core.submit(
            Request::new("POST", "/api/plugins/install_from_url", route, Purpose::PluginInstall)
                .body(body)
                .timeout(INSTALL_TIMEOUT),
        );
        flow.stage = Stage::Installing;
    }

    /// The install finished.
    pub fn on_install(&mut self, core: &mut Core, route: Route, result: &Result<Value, RestError>) {
        let Some(flow) = self.flow.take() else { return };
        match result {
            Ok(v) => {
                let id = v.get("plugin_id").and_then(Value::as_str).unwrap_or_default().to_string();
                let version = v.get("version").and_then(Value::as_str).unwrap_or_default();
                let missing = state::missing_required(&flow.perms, v);
                if missing.is_empty() {
                    core.toast(ToastKind::Success, format!("Installed {} {version}", flow.name));
                } else {
                    core.toast(
                        ToastKind::Warn,
                        format!("Installed {}, but {} was not granted; disabling it", flow.name, missing.join(", ")),
                    );
                    if !id.is_empty() {
                        core.submit(Request::new(
                            "POST",
                            format!("/api/plugins/{}/disable", seg(&id)),
                            route,
                            Purpose::Generic {
                                ok_toast: format!("Disabled {}", flow.name),
                            },
                        ));
                    }
                }
            }
            Err(e) => core.toast_error("Install failed", e),
        }
        core.fetch_now(ResourceId::Plugins);
    }
}

/// The remove dialog: a confirmation with the keep-data toggle.
pub fn remove_dialog(route: Route, id: &str, name: &str) -> Overlay {
    Overlay::Checklist(ChecklistState {
        title: format!("Remove {name}?"),
        items: vec![crate::overlay::CheckItem {
            label: "Keep data (the plugin's logs)".into(),
            checked: false,
            locked: false,
            bit: None,
        }],
        selected: 0,
        footer: Some("Space toggles · Enter removes · Esc cancels".into()),
        ret: Ret::new(route, "ext.remove").with(json!({"id": id, "name": name})),
    })
}
