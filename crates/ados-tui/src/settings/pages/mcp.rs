//! Settings page: ACCESS & SECURITY › MCP. Manage and connect only: the
//! accept-tokens flag, the minted-token registry (`GET /api/mcp/status`),
//! minting a scoped token (`POST /api/mcp/tokens`, shown once and never
//! stored), revoking one or all (`POST /api/mcp/revoke`), and the
//! `claude mcp add` command that connects a local MCP server to this agent.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::Frame;
use serde_json::{json, Value};

use super::kit;
use crate::app::{Core, FrameOut, Hit};
use crate::keymap::{Command, PageKeys};
use crate::overlay::{CheckItem, ChecklistState, Overlay, OverlayResult, Ret, SecretState};
use crate::poll::{ResourceId, Sub};
use crate::prefs::host_for_url;
use crate::settings::config_write;
use crate::settings::page::{Bespoke, BespokeId};
use crate::settings::row::MASK;
use crate::settings::rows_view::{self, Item, RowsState};
use crate::settings::schema::Kind;
use crate::toast::ToastKind;
use crate::worker::{Purpose, Reply, Request, Route};

const ACCEPT_KEY: &str = "mcp.token_accept_enabled";
const ACCEPT_LABEL: &str = "Accept MCP tokens";
/// Scopes this page mints. Flight, destructive and secret-read tokens are
/// minted from Mission Control.
const SCOPES: &[&str] = &["read", "safe_write", "admin"];
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// TTL choices: label and days.
const TTLS: &[(&str, i64)] = &[
    ("1 day", 1),
    ("7 days", 7),
    ("30 days", 30),
    ("90 days", 90),
    ("365 days", 365),
];
const DEFAULT_TTL: usize = 2;
/// Stands in for the operator's local ADOS-MCP clone.
pub const CLONE_PATH: &str = "<path-to>/ADOS-MCP";
/// Stands in for the key when this session has none.
pub const KEY_PLACEHOLDER: &str = "<pairing-key>";

fn route() -> Route {
    Route::page(BespokeId::Mcp.id())
}

/// The LAN-direct `claude mcp add` command for this agent.
pub fn connect_recipe(host: &str, api_key: &str) -> String {
    format!("claude mcp add ados -e ADOS_MCP_AGENT_KEY={api_key} -- node {CLONE_PATH}/dist/index.js --target agent {host}")
}

/// The recipe as displayed: the key masked, or the placeholder when unknown.
pub fn masked_recipe(host: &str, api_key: Option<&str>) -> String {
    connect_recipe(
        host,
        if api_key.is_some() {
            MASK
        } else {
            KEY_PLACEHOLDER
        },
    )
}

/// The agent's reachable address for the recipe: the setup mDNS host, else
/// the LAN host, else the host this session targets, on the API port.
fn connect_host(core: &Core) -> String {
    let s = kit::setup(core);
    let host = kit::text(s, "network.mdns_host")
        .or_else(|| kit::text(s, "lan_host"))
        .map(str::to_string)
        .unwrap_or_else(|| core.target.host.clone());
    let port = s
        .and_then(|v| crate::app::at(v, "network.api_port"))
        .and_then(Value::as_u64)
        .or_else(|| {
            core.target
                .base
                .rsplit(':')
                .next()
                .and_then(|p| p.parse().ok())
        })
        .unwrap_or(8080);
    format!("http://{}:{port}", host_for_url(&host))
}

fn validate_label(s: &str) -> Result<(), String> {
    if s.trim().is_empty() {
        Err("Enter a label, e.g. laptop".into())
    } else {
        Ok(())
    }
}

#[derive(Default)]
pub struct Page {
    rows: RowsState,
    busy: bool,
}

fn status(core: &Core) -> Option<&Value> {
    core.value(&ResourceId::McpStatus)
}

fn tokens(core: &Core) -> Vec<Value> {
    status(core)
        .and_then(|s| s.get("tokens"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// `active`, `expired` or `revoked`.
fn token_state(t: &Value) -> &'static str {
    if t.get("revoked").and_then(Value::as_bool) == Some(true) {
        "revoked"
    } else if t.get("expired").and_then(Value::as_bool) == Some(true) {
        "expired"
    } else {
        "active"
    }
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

impl Page {
    fn items(&self, core: &Core) -> Vec<Item> {
        let t = &core.theme;
        let mut items = vec![Item::Header("Status".into())];
        if let Some(note) = kit::resource_note(core, &ResourceId::McpStatus, "the MCP status") {
            items.push(note);
        }
        let accept = status(core).and_then(|s| s.get("accept_enabled")).cloned();
        let mut row = kit::row(
            ACCEPT_KEY,
            ACCEPT_LABEL,
            "When on, the agent accepts scoped MCP tokens minted for it. Off rejects every MCP token.",
            Kind::Bool,
            accept,
        );
        row.default = Some(json!(false));
        row.read_only = status(core).is_none();
        items.push(Item::Row(row));

        items.push(Item::Header("Tokens".into()));
        let list = tokens(core);
        if list.is_empty() && status(core).is_some() {
            items.push(rows_view::note(
                core,
                "No tokens. Press n to mint one.",
                false,
            ));
        }
        for tok in &list {
            let state = token_state(tok);
            let (glyph, color) = match state {
                "active" => (core.sym.ok, t.success()),
                "expired" => (core.sym.warn, t.warning()),
                _ => (core.sym.err, t.muted()),
            };
            let scopes = tok
                .get("scopes")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            let created = tok
                .get("created_at")
                .and_then(Value::as_f64)
                .map(|s| kit::utc(s as i64))
                .unwrap_or_default();
            let expires = tok
                .get("expires_at")
                .and_then(Value::as_i64)
                .map(kit::utc_ms)
                .unwrap_or_default();
            let label = str_of(tok, "label");
            let label = if label.is_empty() {
                "(no label)"
            } else {
                label
            };
            items.push(Item::Entry {
                key: str_of(tok, "token_id").to_string(),
                line: Line::from(vec![
                    Span::styled(
                        format!("{glyph} {state:<8} "),
                        ratatui::style::Style::default().fg(color),
                    ),
                    Span::styled(format!("{label:<18} "), t.text()),
                    Span::styled(format!("{scopes:<16} "), t.dim()),
                    Span::styled(format!("{created} → {expires}"), t.dim()),
                ]),
            });
        }
        if self.busy {
            items.push(rows_view::note(
                core,
                format!("{} Working…", core.sym.spinner_frame(core.frame as usize)),
                false,
            ));
        }

        items.push(Item::Header("Connect".into()));
        let key = core.client.auth().api_key;
        let recipe = masked_recipe(&connect_host(core), key.as_deref());
        if let Some((head, tail)) = recipe.split_once(" -- ") {
            items.push(Item::Text(Line::from(Span::styled(
                format!("  {head} \\"),
                t.text(),
            ))));
            items.push(Item::Text(Line::from(Span::styled(
                format!("    -- {tail}"),
                t.text(),
            ))));
        }
        items.push(kit::action(
            core,
            "copy",
            "Copy the command",
            "y · with the real key",
        ));
        if key.is_none() {
            items.push(rows_view::note(
                core,
                format!("No pairing key is known for this session: the command keeps the {KEY_PLACEHOLDER} placeholder."),
                true,
            ));
        }
        items.push(rows_view::note(
            core,
            format!("{CLONE_PATH} is your local clone of ADOS-MCP, built once."),
            false,
        ));
        items
    }

    fn focused_token(&self, core: &Core) -> Option<Value> {
        let items = self.items(core);
        let id = self.rows.focused_key(&items)?;
        tokens(core)
            .into_iter()
            .find(|t| str_of(t, "token_id") == id)
    }

    fn toggle_accept(&self, core: &mut Core) {
        let Some(cur) = status(core)
            .and_then(|s| s.get("accept_enabled"))
            .and_then(Value::as_bool)
        else {
            return;
        };
        config_write::submit(
            core,
            route(),
            ACCEPT_KEY,
            json!(!cur),
            &format!("mcp:{ACCEPT_KEY}"),
        );
    }

    fn copy_recipe(core: &mut Core) {
        let host = connect_host(core);
        match core.client.auth().api_key {
            Some(k) => core.copy(&connect_recipe(&host, &k)),
            None => {
                core.copy(&connect_recipe(&host, KEY_PLACEHOLDER));
                core.toast(
                    ToastKind::Warn,
                    format!("No pairing key known: replace {KEY_PLACEHOLDER} in the command"),
                );
            }
        }
    }

    fn mint(core: &mut Core) {
        let mut s = Overlay::input(Ret::new(route(), "label"), "Mint MCP token", "Label", "");
        s.validate = Some(validate_label);
        core.open(Overlay::Input(s));
    }

    fn scopes(core: &mut Core, label: &str) {
        core.open(Overlay::Checklist(ChecklistState {
            title: "Token scopes".into(),
            items: SCOPES
                .iter()
                .map(|s| CheckItem {
                    label: (*s).to_string(),
                    checked: *s == "read",
                    locked: false,
                    bit: None,
                })
                .collect(),
            selected: 0,
            footer: Some(
                "Flight, destructive and secret-read tokens are minted from Mission Control".into(),
            ),
            ret: Ret::new(route(), "scopes").with(json!({ "label": label })),
        }));
    }

    fn revoke(&self, core: &mut Core) {
        let Some(tok) = self.focused_token(core) else {
            core.toast(ToastKind::Info, "Select a token to revoke");
            return;
        };
        if token_state(&tok) == "revoked" {
            core.toast(ToastKind::Info, "That token is already revoked");
            return;
        }
        let label = str_of(&tok, "label").to_string();
        core.open(Overlay::confirm(
            Ret::new(route(), "revoke").with(json!(str_of(&tok, "token_id"))),
            "Revoke MCP token?",
            vec![format!("Clients using {label} lose access at once.")],
        ));
    }

    fn submit_revoke(&mut self, core: &mut Core, body: Value, ok: &str) {
        self.busy = true;
        core.submit(
            Request::new(
                "POST",
                "/api/mcp/revoke",
                route(),
                Purpose::Generic {
                    ok_toast: ok.into(),
                },
            )
            .body(body),
        );
    }
}

impl Bespoke for Page {
    fn subs(&self, _core: &Core) -> Vec<Sub> {
        vec![
            Sub::every_ms(ResourceId::McpStatus, "/api/mcp/status", 5000),
            kit::setup_sub(),
        ]
    }

    fn keys(&self) -> Option<PageKeys> {
        Some(PageKeys::Mcp)
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        let items = self.items(core);
        if kit::nav(&mut self.rows, &items, cmd) {
            return true;
        }
        let focused = self.rows.focused_key(&items).map(str::to_string);
        match cmd {
            Command::Left | Command::Right if focused.as_deref() == Some(ACCEPT_KEY) => {
                self.toggle_accept(core)
            }
            Command::Activate => match focused.as_deref() {
                Some(ACCEPT_KEY) => self.toggle_accept(core),
                Some("copy") => Self::copy_recipe(core),
                _ => {}
            },
            Command::Left | Command::Right => {}
            Command::Mint => Self::mint(core),
            Command::Revoke => self.revoke(core),
            Command::RevokeAll => core.open(Overlay::typed_confirm(
                Ret::new(route(), "revoke_all"),
                "Revoke every MCP token?",
                vec![
                    "Every minted token stops working at once.".into(),
                    "Type revoke to confirm.".into(),
                ],
                "revoke",
            )),
            Command::Copy => Self::copy_recipe(core),
            _ => return false,
        }
        true
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        match (ret.tag, result) {
            ("label", OverlayResult::Text(label)) => Self::scopes(core, label.trim()),
            ("scopes", OverlayResult::Checked(picked)) => {
                let scopes: Vec<&str> = SCOPES
                    .iter()
                    .zip(picked)
                    .filter(|(_, on)| *on)
                    .map(|(s, _)| *s)
                    .collect();
                let label = str_of(&ret.data, "label").to_string();
                if scopes.is_empty() {
                    core.toast(ToastKind::Error, "Pick at least one scope");
                    Self::scopes(core, &label);
                    return;
                }
                let items = TTLS.iter().map(|(l, _)| l.to_string()).collect();
                core.open(Overlay::picker(
                    Ret::new(route(), "ttl").with(json!({ "label": label, "scopes": scopes })),
                    "Token lifetime",
                    items,
                    DEFAULT_TTL,
                ));
            }
            ("ttl", OverlayResult::Picked(i)) => {
                let days = TTLS.get(i).map(|(_, d)| *d).unwrap_or(30);
                let mut body = ret.data.clone();
                body["ttl_ms"] = json!(days * DAY_MS);
                self.busy = true;
                core.submit(
                    Request::new("POST", "/api/mcp/tokens", route(), Purpose::McpMint).body(body),
                );
            }
            ("revoke", OverlayResult::Confirmed) => {
                let id = ret.data.as_str().unwrap_or_default().to_string();
                self.submit_revoke(core, json!({ "token_id": id }), "Token revoked");
            }
            ("revoke_all", OverlayResult::Confirmed) => {
                self.submit_revoke(core, json!({ "all": true }), "Every MCP token revoked");
            }
            _ => {}
        }
    }

    fn on_reply(&mut self, core: &mut Core, reply: Reply) {
        match &reply.purpose {
            Purpose::ConfigSet { row, .. } => {
                config_write::on_reply(core, ACCEPT_LABEL, row, &reply.result);
            }
            Purpose::McpMint => {
                self.busy = false;
                match &reply.result {
                    Ok(body) => match body.get("token").and_then(Value::as_str) {
                        Some(token) => {
                            let expires = body
                                .get("expires_at")
                                .and_then(Value::as_i64)
                                .map(kit::utc_ms);
                            let note = match expires {
                                Some(e) => format!("Copy it now: it is not stored and will not be shown again. Expires {e}."),
                                None => "Copy it now: it is not stored and will not be shown again.".into(),
                            };
                            core.open(Overlay::Secret(SecretState {
                                title: "Token shown once".into(),
                                secret: token.to_string(),
                                note,
                            }));
                        }
                        None => {
                            core.toast(ToastKind::Error, "Mint failed: the agent returned no token")
                        }
                    },
                    Err(e) if e.code() == Some(409) => {
                        core.toast(ToastKind::Error, "Pair this agent first")
                    }
                    Err(e) => core.toast_error("Mint failed", e),
                }
            }
            Purpose::Generic { .. } => self.busy = false,
            _ => return,
        }
        core.fetch_now(ResourceId::McpStatus);
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        let items = self.items(core);
        kit::click(&mut self.rows, &items, hit);
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        let items = self.items(core);
        rows_view::render(f, area, core, &items, &mut self.rows, "mcp:", true, out);
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        let items = self.items(core);
        match self.rows.focused_key(&items)? {
            ACCEPT_KEY => self.rows.focused_row(&items).map(|r| r.help.clone()),
            "copy" => Some("Copies the claude mcp add command with this session's real key".into()),
            _ => Some("x revokes this token · X revokes every token".into()),
        }
    }

    fn animating(&self, _core: &Core) -> bool {
        self.busy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_connect_recipe_matches_mission_control() {
        let host = "http://drone.local:8080";
        assert_eq!(
            connect_recipe(host, "k3y"),
            "claude mcp add ados -e ADOS_MCP_AGENT_KEY=k3y -- node <path-to>/ADOS-MCP/dist/index.js --target agent http://drone.local:8080"
        );
        let shown = masked_recipe(host, Some("k3y"));
        assert!(!shown.contains("k3y"), "{shown}");
        assert!(
            shown.contains(&format!("ADOS_MCP_AGENT_KEY={MASK} ")),
            "{shown}"
        );
        assert!(masked_recipe(host, None).contains("ADOS_MCP_AGENT_KEY=<pairing-key> "));
    }

    #[test]
    fn the_connect_host_prefers_the_mdns_name() {
        let mut app = crate::ui::tests::test_app();
        app.core.res.insert(
            ResourceId::SetupStatus,
            crate::app::Slot {
                value: Some(json!({"network": {"mdns_host": "bench.local", "api_port": 8080}, "lan_host": "192.168.1.50"})),
                error: None,
                code: None,
                error_body: None,
                at: std::time::Instant::now(),
            },
        );
        assert_eq!(connect_host(&app.core), "http://bench.local:8080");
    }

    #[test]
    fn the_page_shows_tokens_and_a_masked_recipe() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = crate::ui::tests::test_app();
        app.core.client.set_api_key(Some("real-key-123".into()));
        let slot = |v: Value| crate::app::Slot {
            value: Some(v),
            error: None,
            code: None,
            error_body: None,
            at: std::time::Instant::now(),
        };
        app.core.res.insert(
            ResourceId::McpStatus,
            slot(json!({"accept_enabled": true, "any_minted": true, "tokens": [
                {"token_id": "t1", "label": "laptop", "scopes": ["read"], "created_at": 1_700_000_000.0,
                 "expires_at": 1_702_592_000_000_i64, "revoked": false, "expired": false}
            ]})),
        );
        app.core.res.insert(
            ResourceId::SetupStatus,
            slot(json!({"network": {"mdns_host": "bench.local", "api_port": 8080}})),
        );
        let mut page = Page::default();
        let mut term = Terminal::new(TestBackend::new(140, 30)).unwrap();
        term.draw(|f| {
            let mut out = FrameOut::default();
            page.render(f, f.area(), &app.core, &mut out);
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        assert!(text.contains("Accept MCP tokens"), "{text}");
        assert!(text.contains("laptop"), "{text}");
        assert!(text.contains("active"), "{text}");
        assert!(
            text.contains(&format!("ADOS_MCP_AGENT_KEY={MASK}")),
            "{text}"
        );
        assert!(
            text.contains("--target agent http://bench.local:8080"),
            "{text}"
        );
        assert!(!text.contains("real-key-123"), "{text}");
    }

    #[test]
    fn token_state_reads_revoked_before_expired() {
        assert_eq!(
            token_state(&json!({"revoked": true, "expired": true})),
            "revoked"
        );
        assert_eq!(
            token_state(&json!({"revoked": false, "expired": true})),
            "expired"
        );
        assert_eq!(
            token_state(&json!({"revoked": false, "expired": false})),
            "active"
        );
    }
}
