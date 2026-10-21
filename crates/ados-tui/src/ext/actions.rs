//! Lifecycle actions on an installed extension: enable, disable, remove,
//! grant and revoke a permission. Each opens its dialog, and the dialog's
//! answer sends the request. Enable, disable, remove and grant are refused
//! unless a fresh snapshot reports disarmed (the agent answers 409 otherwise).

use serde_json::{json, Value};

use super::install::remove_dialog;
use super::state::{self, Install, Perm};
use crate::api_path::seg;
use crate::app::Core;
use crate::overlay::{Overlay, OverlayResult, Ret};
use crate::poll::ResourceId;
use crate::toast::ToastKind;
use crate::worker::{Purpose, Reply, Request, Route};

pub const TAG_ENABLE: &str = "ext.enable";
pub const TAG_DISABLE: &str = "ext.disable";
pub const TAG_REMOVE: &str = "ext.remove";
pub const TAG_GRANT: &str = "ext.grant";
pub const TAG_REVOKE: &str = "ext.revoke";

/// The permissions the detail manifest declares.
fn declared(core: &Core, id: &str) -> Vec<Perm> {
    core.value(&ResourceId::PluginDetail(id.to_string()))
        .and_then(|d| d.get("manifest"))
        .map(state::perms)
        .unwrap_or_default()
}

/// `e` / `d`: confirm, then enable or disable.
pub fn toggle(core: &mut Core, route: Route, inst: &Install, name: &str, enable: bool) {
    if enable && inst.enabled() {
        core.toast(
            ToastKind::Info,
            format!("{name} is already {}", inst.status),
        );
        return;
    }
    if !enable && inst.status == "disabled" {
        core.toast(ToastKind::Info, format!("{name} is already disabled"));
        return;
    }
    if !core.guard_disarmed() {
        return;
    }
    let (tag, verb) = if enable {
        (TAG_ENABLE, "Enable")
    } else {
        (TAG_DISABLE, "Disable")
    };
    let body = if enable {
        vec![format!(
            "{name} {} starts with its granted permissions.",
            inst.version
        )]
    } else {
        vec![format!("{name} stops and stays off until enabled again.")]
    };
    core.open(Overlay::confirm(
        Ret::new(route, tag).with(json!({"id": inst.id, "name": name})),
        format!("{verb} {name}?"),
        body,
    ));
}

/// `x`: the remove dialog with its keep-data toggle.
pub fn remove(core: &mut Core, route: Route, inst: &Install, name: &str) {
    if core.guard_disarmed() {
        core.open(remove_dialog(route, &inst.id, name));
    }
}

/// `g`: grant a declared permission that is not granted.
pub fn grant(core: &mut Core, route: Route, inst: &Install, name: &str) {
    let missing: Vec<Perm> = declared(core, &inst.id)
        .into_iter()
        .filter(|p| !inst.granted(&p.id))
        .collect();
    if missing.is_empty() {
        core.toast(
            ToastKind::Info,
            format!("{name} has every declared permission"),
        );
        return;
    }
    if !core.guard_disarmed() {
        return;
    }
    let ids: Vec<String> = missing.iter().map(|p| p.id.clone()).collect();
    let ret = Ret::new(route, TAG_GRANT).with(json!({"id": inst.id, "name": name, "perms": ids}));
    if missing.len() == 1 {
        let p = &missing[0];
        let mut body = vec![format!("{} ({})", p.title(), p.id)];
        if let Some(d) = &p.description {
            body.push(d.clone());
        }
        core.open(Overlay::confirm(ret, format!("Grant to {name}?"), body));
    } else {
        let items = missing
            .iter()
            .map(|p| format!("{}  ({})", p.title(), p.id))
            .collect();
        core.open(Overlay::picker(ret, format!("Grant to {name}"), items, 0));
    }
}

/// `G`: revoke a granted permission.
pub fn revoke(core: &mut Core, route: Route, inst: &Install, name: &str) {
    let granted: Vec<String> = inst
        .grants
        .iter()
        .filter(|(_, g)| *g)
        .map(|(id, _)| id.clone())
        .collect();
    if granted.is_empty() {
        core.toast(
            ToastKind::Info,
            format!("{name} holds no granted permission"),
        );
        return;
    }
    let ret =
        Ret::new(route, TAG_REVOKE).with(json!({"id": inst.id, "name": name, "perms": granted}));
    core.open(Overlay::picker(
        ret,
        format!("Revoke from {name}"),
        granted,
        0,
    ));
}

fn str_at(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// A lifecycle dialog answered. `true` when `ret` was one of these.
pub fn on_overlay(core: &mut Core, route: Route, ret: &Ret, result: &OverlayResult) -> bool {
    let id = str_at(&ret.data, "id");
    let name = str_at(&ret.data, "name");
    let perm_at = |i: usize| {
        ret.data
            .get("perms")
            .and_then(Value::as_array)
            .and_then(|l| l.get(i))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    match (ret.tag, result) {
        (TAG_ENABLE | TAG_DISABLE, OverlayResult::Confirmed) => {
            if !core.guard_disarmed() {
                return true;
            }
            let (path, toast) = if ret.tag == TAG_ENABLE {
                (
                    format!("/api/plugins/{}/enable", seg(&id)),
                    format!("Enabled {name}"),
                )
            } else {
                (
                    format!("/api/plugins/{}/disable", seg(&id)),
                    format!("Disabled {name}"),
                )
            };
            core.submit(Request::new(
                "POST",
                path,
                route,
                Purpose::Generic { ok_toast: toast },
            ));
        }
        (TAG_REMOVE, OverlayResult::Checked(v)) => {
            if !core.guard_disarmed() {
                return true;
            }
            let keep = if v.first().copied().unwrap_or(false) {
                1
            } else {
                0
            };
            core.submit(Request::new(
                "DELETE",
                format!("/api/plugins/{}?keep_data={keep}", seg(&id)),
                route,
                Purpose::Generic {
                    ok_toast: format!("Removed {name}"),
                },
            ));
        }
        (TAG_GRANT, OverlayResult::Confirmed | OverlayResult::Picked(_)) => {
            let idx = match result {
                OverlayResult::Picked(i) => *i,
                _ => 0,
            };
            let Some(perm) = perm_at(idx) else {
                return true;
            };
            if !core.guard_disarmed() {
                return true;
            }
            core.submit(
                Request::new(
                    "POST",
                    format!("/api/plugins/{}/grant", seg(&id)),
                    route,
                    Purpose::Generic {
                        ok_toast: format!("Granted {perm} to {name}"),
                    },
                )
                .body(json!({"permission_id": perm})),
            );
        }
        (TAG_REVOKE, OverlayResult::Picked(i)) => {
            let Some(perm) = perm_at(*i) else { return true };
            core.submit(Request::new(
                "DELETE",
                format!("/api/plugins/{}/perms/{}", seg(&id), seg(&perm)),
                route,
                Purpose::Page {
                    tag: TAG_REVOKE,
                    data: json!({"id": id, "name": name, "perm": perm}),
                },
            ));
        }
        (TAG_ENABLE | TAG_DISABLE | TAG_REMOVE | TAG_GRANT | TAG_REVOKE, _) => {}
        _ => return false,
    }
    true
}

/// A lifecycle request finished: refresh what it changed.
pub fn on_reply(core: &mut Core, reply: &Reply) {
    if let Purpose::Page {
        tag: TAG_REVOKE,
        data,
    } = &reply.purpose
    {
        let perm = str_at(data, "perm");
        let name = str_at(data, "name");
        match &reply.result {
            Ok(v) => {
                core.toast(ToastKind::Success, format!("Revoked {perm} from {name}"));
                if v.get("requires_restart").and_then(Value::as_bool) == Some(true) {
                    core.toast(
                        ToastKind::Warn,
                        format!("Restart {name} to apply the revocation"),
                    );
                }
            }
            Err(e) => core.toast_error(&format!("Revoke {perm}"), e),
        }
    }
    core.fetch_now(ResourceId::Plugins);
}
