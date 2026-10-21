//! The showcase frames: each starts from a fresh app, is driven with real
//! keys, and is drawn at 120×40.

use std::time::{Duration, Instant};

use ados_protocol::rest::RestError;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use tui_input::Input;

use super::export::{HEIGHT, WIDTH};
use super::fixtures::{self as fx, base, land, put, LAN_IP};
use super::pages;
use crate::app::{App, Tab};
use crate::args::Target;
use crate::model::ProfileRoutes;
use crate::overlay::{AuthMode, AuthState, Overlay};
use crate::poll::{PollResult, ResourceId};
use crate::theme::ThemeId;
use crate::ui::logs::Mode;
use crate::worker::{Purpose, Reply, Route};

/// Every frame, in replay order.
pub(super) const NAMES: [&str; 18] = [
    "overview",
    "vehicle",
    "link",
    "video",
    "params",
    "params-write",
    "extensions",
    "logs",
    "system",
    "settings-appearance",
    "settings-wifi",
    "settings-mcp",
    "palette",
    "help",
    "remote-locked",
    "theme-light",
    "theme-nvg",
    "ground",
];

fn key(app: &mut App, code: KeyCode) {
    app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn typed(app: &mut App, text: &str) {
    text.chars().for_each(|c| key(app, KeyCode::Char(c)));
}

fn palette(app: &mut App, query: &str) {
    app.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
    typed(app, query);
}

fn draw(app: &mut App) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
    terminal
        .draw(|f| {
            let _ = crate::ui::render(f, app);
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

fn drone() -> App {
    base("drone", ThemeId::Amber)
}

/// Build the frame called `name`.
pub(super) fn build(name: &str) -> Buffer {
    let mut app = match name {
        "overview" => drone(),
        "vehicle" => vehicle(),
        "link" => link(),
        "video" => video(),
        "params" => params(),
        "params-write" => {
            let mut app = params();
            typed(&mut app, "w");
            app
        }
        "extensions" => extensions(),
        "logs" => logs(),
        "system" => system(),
        "settings-appearance" => {
            let mut app = drone();
            typed(&mut app, "9");
            app
        }
        "settings-wifi" => wifi(),
        "settings-mcp" => mcp(),
        "palette" => {
            let mut app = drone();
            palette(&mut app, "wifi");
            app
        }
        "help" => {
            let mut app = drone();
            typed(&mut app, "?");
            app
        }
        "remote-locked" => remote_locked(),
        "theme-light" => base("drone", ThemeId::BrandLight),
        "theme-nvg" => base("drone", ThemeId::Nvg),
        "ground" => ground(),
        other => panic!("unknown showcase frame {other}"),
    };
    draw(&mut app)
}

fn vehicle() -> App {
    let mut app = drone();
    typed(&mut app, "2");
    land(&mut app, ResourceId::Snapshot, fx::snapshot());
    land(&mut app, ResourceId::Battery, fx::battery());
    for i in 0..60 {
        land(&mut app, ResourceId::Telemetry, fx::telemetry(i));
    }
    app
}

fn link() -> App {
    let mut app = drone();
    typed(&mut app, "3");
    for i in 0..60 {
        land(&mut app, ResourceId::Wfb, fx::wfb(i));
    }
    for n in 0..2 {
        land(&mut app, ResourceId::Lora, fx::lora(n));
    }
    land(&mut app, ResourceId::LoraBeacons, fx::lora_beacons());
    app
}

fn video() -> App {
    let mut app = drone();
    typed(&mut app, "4");
    for (id, v) in fx::video() {
        land(&mut app, id, v);
    }
    for i in 0..60 {
        land(&mut app, ResourceId::VideoLatency, fx::latency(i));
    }
    app
}

/// Params with the ATC group selected and `ATC_RAT_RLL_P` staged
/// 0.135 → 0.136.
fn params() -> App {
    let mut app = drone();
    typed(&mut app, "5");
    land(&mut app, ResourceId::Snapshot, fx::snapshot());
    land(&mut app, ResourceId::Params, fx::params());
    // The metadata catalog decodes on a background thread.
    let deadline = Instant::now() + Duration::from_secs(20);
    while app.tabs.params.animating(&app.core) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        app.tabs.params.tick(&mut app.core);
    }
    // The sidebar: All, Staged, Non-default, Favourites, then the groups (ATC
    // sorts first).
    key(&mut app, KeyCode::Left);
    for _ in 0..4 {
        key(&mut app, KeyCode::Down);
    }
    key(&mut app, KeyCode::Right);
    let row = fx::PARAMS
        .iter()
        .filter(|(n, _)| n.starts_with("ATC_"))
        .position(|(n, _)| *n == "ATC_RAT_RLL_P")
        .unwrap();
    for _ in 0..row {
        key(&mut app, KeyCode::Down);
    }
    key(&mut app, KeyCode::Enter);
    key(&mut app, KeyCode::Backspace);
    typed(&mut app, "6");
    key(&mut app, KeyCode::Enter);
    app
}

fn extensions() -> App {
    let mut app = drone();
    for (id, v) in pages::extensions() {
        put(&mut app, id, v);
    }
    typed(&mut app, "6");
    app
}

fn logs() -> App {
    let mut app = drone();
    typed(&mut app, "7");
    let tab = &mut app.tabs.logs;
    for line in pages::log_lines() {
        tab.ring.push(line);
    }
    tab.mode = Mode::Streaming;
    app
}

fn system() -> App {
    let mut app = drone();
    typed(&mut app, "8");
    for (id, v) in pages::system() {
        put(&mut app, id, v);
    }
    for i in 0..60 {
        land(&mut app, ResourceId::System, pages::system_load(i));
    }
    app
}

fn wifi() -> App {
    let mut app = drone();
    typed(&mut app, "9");
    palette(&mut app, "wifi");
    key(&mut app, KeyCode::Enter);
    for (id, v) in pages::wifi() {
        put(&mut app, id, v);
    }
    typed(&mut app, "s");
    let reply = Reply {
        route: Route::page("wifi"),
        purpose: Purpose::WifiScan,
        result: Ok(pages::wifi_scan()),
    };
    app.tabs.settings.on_reply(&mut app.core, reply);
    app
}

fn mcp() -> App {
    let mut app = drone();
    app.core.client.set_api_key(Some("showcase-key".into()));
    typed(&mut app, "9");
    palette(&mut app, "mcp");
    key(&mut app, KeyCode::Enter);
    for (id, v) in pages::mcp() {
        put(&mut app, id, v);
    }
    app
}

/// A remote agent that refuses this session's credentials.
fn remote_locked() -> App {
    let mut app = drone();
    app.core.target = Target::from_host(None, LAN_IP, 8080);
    let refused = PollResult {
        setup: Err(RestError::Status {
            code: 401,
            body: None,
        }),
        fc_status: None,
        status_full: None,
        profile: ProfileRoutes::default(),
    };
    app.core
        .view
        .apply(refused, Instant::now(), "12:00:02".into());
    app.core.overlays.push(Overlay::Auth(AuthState {
        mode: AuthMode::Key,
        input: Input::default(),
        message: Some("API key required".into()),
        offer_remember: false,
    }));
    app
}

fn ground() -> App {
    let mut app = base("ground_station", ThemeId::Amber);
    typed(&mut app, "2");
    for (id, v) in pages::ground() {
        put(&mut app, id, v);
    }
    app
}
