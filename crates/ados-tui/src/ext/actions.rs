//! Lifecycle actions on an installed extension: enable, disable, remove,
//! grant and revoke a permission. Each opens its dialog, and the dialog's
//! answer sends the request. Every one confirms first and is refused unless a
//! fresh snapshot reports disarmed (a revoke takes effect at once, so a
//! flying skill would lose the permission mid-flight).

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
/// The picked permission's revoke, awaiting its confirm.
pub const TAG_REVOKE_CONFIRM: &str = "ext.revoke.confirm";

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

/// `G`: revoke a granted permission, confirmed.
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
    if !core.guard_disarmed() {
        return;
    }
    match granted.as_slice() {
        [perm] => confirm_revoke(core, route, &inst.id, name, perm),
        _ => {
            let ret = Ret::new(route, TAG_REVOKE)
                .with(json!({"id": inst.id, "name": name, "perms": granted}));
            core.open(Overlay::picker(
                ret,
                format!("Revoke from {name}"),
                granted,
                0,
            ));
        }
    }
}

fn confirm_revoke(core: &mut Core, route: Route, id: &str, name: &str, perm: &str) {
    let ret =
        Ret::new(route, TAG_REVOKE_CONFIRM).with(json!({"id": id, "name": name, "perm": perm}));
    let body = vec![
        perm.to_string(),
        "The extension loses it at once, in flight too.".to_string(),
    ];
    core.open(Overlay::confirm(ret, format!("Revoke from {name}?"), body));
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
            confirm_revoke(core, route, &id, &name, &perm);
        }
        (TAG_REVOKE_CONFIRM, OverlayResult::Confirmed) => {
            if !core.guard_disarmed() {
                return true;
            }
            let perm = str_at(&ret.data, "perm");
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
        (
            TAG_ENABLE | TAG_DISABLE | TAG_REMOVE | TAG_GRANT | TAG_REVOKE | TAG_REVOKE_CONFIRM,
            _,
        ) => {}
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::app::TabId;
    use crate::model::ProfileRoutes;
    use crate::ui::tests::{feed, test_app};

    const ROUTE: Route = Route::tab(TabId::Extensions);

    fn inst(grants: &[&str]) -> Install {
        Install {
            id: "com.example.follow".into(),
            version: "1.0.0".into(),
            status: "running".into(),
            signer: None,
            failure_reason: None,
            revocation_pending: false,
            grants: grants.iter().map(|g| (g.to_string(), true)).collect(),
        }
    }

    fn setup(armed: bool) -> Value {
        json!({
            "version": "0.99.108", "device_name": "ados-x", "profile": "drone", "paired": true,
            "mavlink": {"connected": true},
            "telemetry": {"mode": "LOITER", "armed": armed}
        })
    }

    #[test]
    fn a_revoke_is_refused_while_armed() {
        let mut app = test_app();
        feed(
            &mut app,
            setup(true),
            ProfileRoutes::default(),
            Duration::ZERO,
        );
        revoke(
            &mut app.core,
            ROUTE,
            &inst(&["mavlink.command", "video"]),
            "Follow",
        );
        assert!(app.core.overlays.is_empty(), "no picker opens while armed");
    }

    #[test]
    fn a_picked_permission_is_confirmed_before_it_is_revoked() {
        let mut app = test_app();
        feed(
            &mut app,
            setup(false),
            ProfileRoutes::default(),
            Duration::ZERO,
        );
        revoke(
            &mut app.core,
            ROUTE,
            &inst(&["mavlink.command", "video"]),
            "Follow",
        );
        let Some(Overlay::Picker(_)) = app.core.overlays.pop() else {
            panic!("two grants open a picker");
        };
        let ret = Ret::new(ROUTE, TAG_REVOKE)
            .with(json!({"id": "com.example.follow", "name": "Follow", "perms": ["mavlink.command", "video"]}));
        assert!(on_overlay(
            &mut app.core,
            ROUTE,
            &ret,
            &OverlayResult::Picked(0)
        ));
        let Some(Overlay::Confirm(c)) = app.core.overlays.pop() else {
            panic!("the pick asks to confirm");
        };
        assert_eq!(c.ret.tag, TAG_REVOKE_CONFIRM);
        assert_eq!(c.ret.data["perm"], "mavlink.command");
    }
}
