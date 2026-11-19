//! The Ground tab's operator actions: phone invites, the RTK base survey-in,
//! and taking the radio uplink back from a phone.

use serde_json::{json, Value};

use super::{text, GroundTab, ROUTE};
use crate::api_path::seg;
use crate::app::Core;
use crate::overlay::{Overlay, Ret};
use crate::poll::ResourceId;
use crate::toast::ToastKind;
use crate::worker::{Purpose, Request};

/// The uplink handover route: `GET` reads `{uplink}`, `POST {op}` moves it.
pub(super) const UPLINK_PATH: &str = "/api/v1/ground-station/uplink";

/// The phone holding this box's uplink (its name, else its device id), or
/// `None` while the box transmits or the radio service is not reporting.
pub(super) fn uplink_holder(core: &Core) -> Option<String> {
    let u = core
        .value(&ResourceId::Uplink)?
        .get("uplink")
        .filter(|u| u.is_object())?;
    if u.get("state").and_then(Value::as_str) != Some("standby") {
        return None;
    }
    let name = |k: &str| u.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    Some(
        name("holder_name")
            .or_else(|| name("holder_device_id"))
            .unwrap_or("a phone")
            .to_string(),
    )
}

impl GroundTab {
    pub(super) fn approve(&mut self, core: &mut Core) {
        let Some(inv) = self.invite(core) else {
            core.toast(ToastKind::Info, "No pending invite");
            return;
        };
        let body = vec![
            format!("Label: {}", text(&inv, "label")),
            format!("Phone fingerprint: {}", text(&inv, "phone_fingerprint")),
            "Check the fingerprint matches the phone before approving.".into(),
        ];
        let ret = Ret::new(ROUTE, "approve").with(json!({ "id": text(&inv, "invite_id") }));
        core.open(Overlay::confirm(ret, "Approve the phone receiver?", body));
    }

    pub(super) fn reject(&mut self, core: &mut Core) {
        let Some(inv) = self.invite(core) else {
            core.toast(ToastKind::Info, "No pending invite");
            return;
        };
        let id = text(&inv, "invite_id").to_string();
        core.submit(Request::new(
            "POST",
            format!("/api/v1/ground-station/wfb/invite/{}/reject", seg(&id)),
            ROUTE,
            Purpose::Generic {
                ok_toast: "Invite rejected".into(),
            },
        ));
    }

    /// Ask before taking the uplink back: the phone holding it is told to
    /// stop, then this box transmits to the fleet again.
    pub(super) fn take_back(&mut self, core: &mut Core) {
        let Some(holder) = uplink_holder(core) else {
            let why = match core
                .value(&ResourceId::Uplink)
                .and_then(|v| v.get("uplink"))
                .and_then(|u| u.get("state"))
                .and_then(Value::as_str)
            {
                Some("transmitting") => "This ground station already transmits",
                _ => "The radio service is not reporting the uplink",
            };
            core.toast(ToastKind::Info, why);
            return;
        };
        let body = vec![
            format!("{holder} holds the uplink; this ground station only receives."),
            format!("{holder} is told to stop transmitting, then this ground station transmits to the fleet again."),
            "It takes the uplink back even if the phone does not answer.".into(),
        ];
        core.open(Overlay::confirm(
            Ret::new(ROUTE, "take_back"),
            "Take the uplink back?",
            body,
        ));
    }

    pub(super) fn submit_take_back(core: &mut Core) {
        core.submit(
            Request::new(
                "POST",
                UPLINK_PATH,
                ROUTE,
                Purpose::Page {
                    tag: "take_back",
                    data: Value::Null,
                },
            )
            .body(json!({ "op": "take_back" })),
        );
    }

    pub(super) fn survey_in(&mut self, core: &mut Core) {
        let base = core.value(&ResourceId::Gnss).and_then(|g| g.get("base"));
        let dur = base
            .and_then(|b| b.get("min_dur_s"))
            .and_then(Value::as_u64)
            .unwrap_or(300);
        let mut input = Overlay::input(
            Ret::new(ROUTE, "survey_dur"),
            "Start survey-in",
            "Minimum duration (s)",
            &dur.to_string(),
        );
        input.validate = Some(valid_duration);
        core.open(Overlay::Input(input));
    }

    pub(super) fn base_request(core: &mut Core, body: Value, ok: &str) {
        core.submit(
            Request::new(
                "PUT",
                "/api/v1/gnss/base",
                ROUTE,
                Purpose::Page {
                    tag: "base",
                    data: json!({ "ok": ok }),
                },
            )
            .body(body),
        );
    }
}

fn valid_duration(s: &str) -> Result<(), String> {
    match s.trim().parse::<u32>() {
        Ok(n) if n >= 1 => Ok(()),
        _ => Err("A whole number of seconds, 1 or more".into()),
    }
}

pub(super) fn valid_accuracy(s: &str) -> Result<(), String> {
    match s.trim().parse::<f64>() {
        Ok(v) if v.is_finite() && v > 0.0 => Ok(()),
        _ => Err("A distance in metres, above 0".into()),
    }
}
