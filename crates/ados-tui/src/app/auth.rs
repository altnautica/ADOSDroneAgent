//! Remote-agent authentication: the API-key prompt (paired agents answer 401
//! without a key) and the dashboard PIN prompt (unpaired agents answer 403
//! off-box until a PIN session is presented).
//!
//! The shell stays drawn behind the prompt and every tab keeps working; data
//! panels simply read the failure until a key or PIN is accepted.

use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tui_input::Input;

use super::{Core, TabId};
use crate::overlay::{AuthMode, AuthState, Overlay};
use crate::prefs::SavedTarget;
use crate::toast::ToastKind;
use crate::worker::{Purpose, Reply, Request, Route};

const TAG_PAIRING: &str = "auth.pairing_info";
const TAG_KEY_TEST: &str = "auth.key_test";
const TAG_PIN_STATUS: &str = "auth.pin_status";
const TAG_PIN_VERIFY: &str = "auth.pin_verify";

/// What the session knows about the agent's auth posture.
#[derive(Debug, Default)]
pub struct AuthCtl {
    /// `pairing/info.paired`, once read.
    pub paired: Option<bool>,
    /// A rejected answer is being handled (avoid stacking prompts).
    busy: bool,
    /// The operator closed the prompt: do not reopen it on every poll. A
    /// manual refresh clears this.
    pub dismissed: bool,
    /// When a key or PIN session was last set. Requests already in flight
    /// went out without it, so their refusals are not about the new one.
    changed_at: Option<Instant>,
}

/// How long after a credential change a refusal is assumed to be from a
/// request that left before it (one request timeout).
const IN_FLIGHT: Duration = Duration::from_secs(8);

fn mark_changed(core: &mut Core) {
    core.auth.changed_at = Some(Instant::now());
}

fn req(method: &'static str, path: &str, tag: &'static str) -> Request {
    Request::new(
        method,
        path,
        Route::tab(TabId::Overview),
        Purpose::Page {
            tag,
            data: Value::Null,
        },
    )
}

/// Whether a reply belongs to this module.
pub fn owns(reply: &Reply) -> bool {
    matches!(&reply.purpose, Purpose::Page { tag, .. } if tag.starts_with("auth."))
}

/// On a remote target, read the public pairing state first.
pub fn start(core: &mut Core) {
    if core.target.remote {
        core.submit(req("GET", "/api/pairing/info", TAG_PAIRING));
    }
}

fn auth_open(core: &Core) -> bool {
    core.overlays.iter().any(|o| matches!(o, Overlay::Auth(_)))
}

fn open(core: &mut Core, mode: AuthMode, message: Option<String>) {
    // Replace an open prompt rather than stacking a second one.
    core.overlays.retain(|o| !matches!(o, Overlay::Auth(_)));
    core.open(Overlay::Auth(AuthState {
        mode,
        input: Input::default(),
        message,
        offer_remember: false,
    }));
}

/// A request was refused with `code` (401 or 403). `read` is true for polls
/// and resource reads, false for a write the operator asked for.
///
/// A 401 means the key (or an expired PIN session) is missing. A 403 is a PIN
/// prompt only for a read on an unpaired agent: the agent also answers 403 for
/// refusals that have nothing to do with the dashboard PIN (an extension
/// lifecycle refusal, an MCP scope), and those must not drop a working
/// session.
pub fn on_denied(core: &mut Core, code: u16, read: bool) {
    if auth_open(core) || core.auth.busy || core.auth.dismissed {
        return;
    }
    if core
        .auth
        .changed_at
        .is_some_and(|t| t.elapsed() < IN_FLIGHT)
    {
        return;
    }
    let unpaired = core.auth.paired == Some(false);
    let has_session = core.client.auth().session.is_some();
    let pin = match code {
        403 => read && unpaired,
        401 => unpaired && has_session,
        _ => return,
    };
    if code == 403 && !pin {
        return;
    }
    if pin {
        // A PIN session is needed (or the one held expired).
        if has_session {
            core.client.set_session(None);
        }
        core.auth.busy = true;
        core.submit(req("GET", "/api/dashboard/pin/status", TAG_PIN_STATUS));
    } else {
        open(core, AuthMode::Key, Some("API key required".into()));
    }
}

/// The prompt was submitted.
pub fn submit(core: &mut Core, text: String) {
    let mode = match core.overlays.last() {
        Some(Overlay::Auth(a)) => a.mode,
        _ => return,
    };
    let text = text.trim().to_string();
    match mode {
        AuthMode::Key => {
            if text.is_empty() {
                set_message(core, "Enter the pairing key");
                return;
            }
            core.client.set_api_key(Some(text.clone()));
            core.auth.busy = true;
            let mut r = req("GET", "/api/status", TAG_KEY_TEST);
            r.purpose = Purpose::Page {
                tag: TAG_KEY_TEST,
                data: json!(text),
            };
            core.submit(r);
            set_message(core, "Checking…");
        }
        AuthMode::Pin => {
            if !(4..=12).contains(&text.len()) || !text.chars().all(|c| c.is_ascii_digit()) {
                set_message(core, "The PIN is 4 to 12 digits");
                return;
            }
            core.auth.busy = true;
            core.submit(
                req("POST", "/api/dashboard/pin/verify", TAG_PIN_VERIFY)
                    .body(json!({ "pin": text })),
            );
            set_message(core, "Checking…");
        }
        AuthMode::Blocked => {}
    }
}

fn set_message(core: &mut Core, msg: &str) {
    if let Some(Overlay::Auth(a)) = core.overlays.last_mut() {
        a.message = Some(msg.to_string());
        if msg != "Checking…" {
            a.input = Input::default();
        }
    }
}

/// The "remember this key" question was answered.
pub fn remember(core: &mut Core, yes: bool) {
    core.overlays.retain(|o| !matches!(o, Overlay::Auth(_)));
    if yes {
        let key = core.client.auth().api_key;
        let (host, port) = crate::args::split_host_port(&core.target.base)
            .unwrap_or((core.target.host.clone(), crate::args::DEFAULT_PORT));
        let name = core.target.name.clone().unwrap_or_else(|| host.clone());
        let targets = &mut core.store.prefs.targets;
        match targets.iter_mut().find(|t| t.name == name) {
            Some(t) => t.api_key = key,
            None => targets.push(SavedTarget {
                name: name.clone(),
                host,
                port,
                api_key: key,
            }),
        }
        core.target.name = Some(name.clone());
        core.save_prefs();
        core.toast(ToastKind::Success, format!("Key saved for {name}"));
    }
    core.refetch_all();
}

fn unix_to_hms(secs: f64) -> String {
    let s = (secs.max(0.0) as u64) % 86_400;
    format!("{:02}:{:02}:{:02} UTC", s / 3600, (s % 3600) / 60, s % 60)
}

/// A worker reply tagged `auth.*`.
pub fn on_reply(core: &mut Core, reply: Reply) {
    let Purpose::Page { tag, data } = reply.purpose else {
        return;
    };
    match tag {
        TAG_PAIRING => {
            if let Ok(v) = &reply.result {
                core.auth.paired = v.get("paired").and_then(Value::as_bool);
            }
        }
        TAG_KEY_TEST => {
            core.auth.busy = false;
            match reply.result {
                Ok(_) => {
                    if let Some(Overlay::Auth(a)) = core.overlays.last_mut() {
                        a.offer_remember = true;
                        a.message = None;
                    }
                    mark_changed(core);
                    core.refetch_all();
                    let _ = data;
                }
                Err(e) if matches!(e.code(), Some(401) | Some(403)) => {
                    core.client.set_api_key(None);
                    set_message(core, "Key rejected");
                }
                Err(e) => set_message(
                    core,
                    &format!("Could not check the key: {}", super::error_text(&e)),
                ),
            }
        }
        TAG_PIN_STATUS => {
            core.auth.busy = false;
            let v = reply.result.unwrap_or(Value::Null);
            if v.get("pin_set").and_then(Value::as_bool) == Some(false) {
                open(
                    core,
                    AuthMode::Blocked,
                    Some("No dashboard PIN is set. Set one on the agent (on-box TUI or web dashboard) first.".into()),
                );
            } else if v.get("locked").and_then(Value::as_bool) == Some(true) {
                let until = v
                    .get("locked_until")
                    .and_then(Value::as_f64)
                    .map(unix_to_hms);
                open(
                    core,
                    AuthMode::Blocked,
                    Some(format!(
                        "Locked until {}",
                        until.unwrap_or_else(|| "later".into())
                    )),
                );
            } else {
                open(core, AuthMode::Pin, Some("Dashboard PIN required".into()));
            }
        }
        TAG_PIN_VERIFY => {
            core.auth.busy = false;
            match reply.result {
                Ok(v) => match v.get("session").and_then(Value::as_str) {
                    Some(session) => {
                        core.client.set_session(Some(session.to_string()));
                        mark_changed(core);
                        core.overlays.retain(|o| !matches!(o, Overlay::Auth(_)));
                        core.toast(ToastKind::Success, "PIN accepted");
                        core.refetch_all();
                    }
                    None => set_message(core, "The agent returned no session"),
                },
                Err(e) => {
                    let body = e.body().cloned().unwrap_or(Value::Null);
                    let msg = match e.code() {
                        Some(401) => match body.get("remaining_attempts").and_then(Value::as_u64) {
                            Some(n) => format!("Wrong PIN, {n} attempts left"),
                            None => "Wrong PIN".into(),
                        },
                        Some(429) => format!(
                            "Locked until {}",
                            body.get("locked_until")
                                .and_then(Value::as_f64)
                                .map(unix_to_hms)
                                .unwrap_or_else(|| "later".into())
                        ),
                        Some(423) => format!(
                            "Node locked, retry in {} s",
                            body.get("retry_after_s")
                                .and_then(Value::as_u64)
                                .unwrap_or(0)
                        ),
                        _ => super::error_text(&e),
                    };
                    set_message(core, &msg);
                }
            }
        }
        _ => {}
    }
}
