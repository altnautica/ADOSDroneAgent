//! ADOS Drone Agent terminal dashboard.
//!
//! Launched by `ados` with no subcommand. Polls the agent's
//! `/api/v1/setup/status` endpoint every two seconds and renders the same
//! information the previous Python `rich` dashboard showed. Read-only.

mod model;
mod theme;
mod ui;

use std::io::Stdout;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ados_protocol::rest::RestClient;
use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::crossterm::{execute, ExecutableCommand};
use ratatui::Terminal;
use serde_json::Value;

use crate::model::{Dashboard, History};

/// Where the agent stores the pairing key (matches `ados.core.paths.PAIRING_JSON`).
const PAIRING_JSON: &str = "/etc/ados/pairing.json";

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_millis(100);

fn load_api_key() -> Option<String> {
    let text = std::fs::read_to_string(PAIRING_JSON).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value
        .get("api_key")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// UTC HH:MM:SS for the "refreshed" indicator. (A timezone-aware clock would
/// need a dependency; the indicator only needs to show that data is updating.)
fn now_hms() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Restore the terminal out of raw mode and the alternate screen.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen, Show);
}

/// Restore the terminal from a panic hook before the default hook runs, so a
/// panic in the render loop never strands the operator's shell in raw mode +
/// the alternate screen. The release profile aborts on panic, but a hook still
/// runs before the abort.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default(info);
    }));
}

fn main() -> Result<()> {
    let mut client = RestClient::local();
    if let Some(key) = load_api_key() {
        client = client.with_api_key(key);
    }

    install_panic_hook();
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let result = run(&mut terminal, &client);

    // Always restore the terminal on a clean exit too.
    restore_terminal();
    let _ = terminal.show_cursor();
    result
}

fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>, client: &RestClient) -> Result<()> {
    let mut data: Option<Value> = None;
    let mut error: Option<String> = None;
    let mut refreshed = now_hms();
    let mut last_fetch: Option<Instant> = None;
    // Trend buffers of verified telemetry, one sample per successful poll.
    let mut history = History::default();

    loop {
        // Fetch immediately on first iteration, then every POLL_INTERVAL.
        let due = match last_fetch {
            None => true,
            Some(t) => t.elapsed() >= POLL_INTERVAL,
        };
        if due {
            match client.setup_status() {
                Ok(v) => {
                    history.record(&Dashboard::from_status(&v));
                    data = Some(v);
                    error = None;
                }
                Err(e) => error = Some(format!("Agent unreachable: {e}")),
            }
            refreshed = now_hms();
            last_fetch = Some(Instant::now());
        }

        let dash = data.as_ref().map(Dashboard::from_status);
        terminal.draw(|f| ui::render(f, dash.as_ref(), &history, &refreshed, error.as_deref()))?;

        if event::poll(TICK)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    let quit = matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q'))
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL));
                    if quit {
                        return Ok(());
                    }
                }
            }
        }
    }
}
