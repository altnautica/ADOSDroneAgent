//! ADOS Drone Agent terminal dashboard.
//!
//! Launched by `ados` with no subcommand. Polls the agent's
//! `/api/v1/setup/status` endpoint every two seconds and renders the same
//! information the previous Python `rich` dashboard showed. Read-only.

mod action;
mod model;
mod theme;
mod ui;
mod update;

use std::io::{Stdout, Write};
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

use crate::action::{update_request, Action, ACTIONS};
use crate::model::{Dashboard, History};

/// Where the agent stores the pairing key (matches `ados.core.paths.PAIRING_JSON`).
const PAIRING_JSON: &str = "/etc/ados/pairing.json";

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_millis(100);
/// Past this age with no successful poll, the shown snapshot is flagged stale so
/// a departed agent never keeps reading live under a moving clock.
const STALE_AFTER: Duration = Duration::from_secs(6);

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

/// Put the cockpit back after a shell-out: raw mode, alt screen, full redraw.
fn restore_cockpit(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    enable_raw_mode()?;
    std::io::stdout().execute(EnterAlternateScreen)?;
    terminal.clear()?;
    Ok(())
}

fn read_line() -> String {
    let mut buf = String::new();
    let _ = std::io::stdin().read_line(&mut buf);
    buf
}

/// Hold the plain terminal until the operator presses Enter, then restore the
/// cockpit.
fn pause_then_restore(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    print!("\nPress Enter to return to the dashboard… ");
    let _ = std::io::stdout().flush();
    let _ = read_line();
    restore_cockpit(terminal)
}

/// Show why a request was refused on the plain terminal, in the same frame an
/// action's own output would appear in, so the refusal is not a silent no-op.
fn show_refusal(terminal: &mut Terminal<CrosstermBackend<Stdout>>, why: &str) -> Result<()> {
    restore_terminal();
    println!("\n{why}");
    pause_then_restore(terminal)
}

/// Run an update request: the confirming update action, or the refusal while the
/// vehicle reports armed.
fn request_update(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    armed: Option<bool>,
) -> Result<()> {
    match update_request(armed) {
        Ok(action) => run_action(terminal, action),
        Err(why) => show_refusal(terminal, why),
    }
}

/// Whether a key pressed on the launch update splash asks for the update. Only
/// `u`: every other key, Enter included, is the "later" the splash offers.
fn splash_requests_update(code: KeyCode) -> bool {
    matches!(code, KeyCode::Char('u') | KeyCode::Char('U'))
}

/// Run a quick action by shelling out to the real terminal (Pattern A). The
/// cockpit leaves the alt screen so the command's own output — and any sudo
/// prompt or the command's own confirmation — is visible, optionally confirms
/// first, then restores the cockpit. The command shells an existing `ados` (or
/// `systemctl`) verb, so no write path to the agent is opened here.
fn run_action(terminal: &mut Terminal<CrosstermBackend<Stdout>>, action: &Action) -> Result<()> {
    restore_terminal();
    // The terminal is now in cooked mode (Ctrl-C raises SIGINT). Ignore SIGINT +
    // SIGQUIT in this process for the duration of the action so a Ctrl-C used to
    // stop a streaming child (e.g. `ados logs tail`, whose only exit IS Ctrl-C)
    // exits the child, not the whole cockpit. `spawn_action` resets them to
    // default in the child so it still receives them. Restored on return (RAII).
    #[cfg(unix)]
    let _signals = SignalGuard::ignore();

    if action.confirm {
        print!("\n{} — proceed? [y/N] ", action.label);
        let _ = std::io::stdout().flush();
        if !read_line().trim().eq_ignore_ascii_case("y") {
            return restore_cockpit(terminal);
        }
    }

    println!("\n$ {} {}\n", action.program, action.args.join(" "));
    match spawn_action(action.program, action.args) {
        Ok(status) if !status.success() => {
            println!("\n[exited with status {}]", status.code().unwrap_or(-1));
        }
        Err(e) => println!("\n[could not run {}: {e}]", action.program),
        _ => {}
    }
    pause_then_restore(terminal)
}

/// Spawn a quick-action child inheriting this terminal, resetting SIGINT/SIGQUIT
/// to their default disposition in the child (they are ignored in the parent for
/// the child's duration) so a Ctrl-C reaches the child but not this cockpit.
fn spawn_action(program: &str, args: &[&str]) -> std::io::Result<std::process::ExitStatus> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    #[cfg(unix)]
    // SAFETY: the pre_exec closure runs in the forked child before exec and only
    // calls the async-signal-safe `signal(2)` to restore default dispositions.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            nix::libc::signal(nix::libc::SIGINT, nix::libc::SIG_DFL);
            nix::libc::signal(nix::libc::SIGQUIT, nix::libc::SIG_DFL);
            Ok(())
        });
    }
    cmd.status()
}

/// Ignores SIGINT + SIGQUIT in this process for its lifetime and restores the
/// previous dispositions on drop (the POSIX `system(3)` pattern).
#[cfg(unix)]
struct SignalGuard {
    int: nix::libc::sighandler_t,
    quit: nix::libc::sighandler_t,
}

#[cfg(unix)]
impl SignalGuard {
    fn ignore() -> Self {
        // SAFETY: `signal(2)` just swaps the disposition and returns the prior
        // one, which Drop restores.
        unsafe {
            SignalGuard {
                int: nix::libc::signal(nix::libc::SIGINT, nix::libc::SIG_IGN),
                quit: nix::libc::signal(nix::libc::SIGQUIT, nix::libc::SIG_IGN),
            }
        }
    }
}

#[cfg(unix)]
impl Drop for SignalGuard {
    fn drop(&mut self) {
        // SAFETY: restoring the dispositions captured in `ignore`.
        unsafe {
            nix::libc::signal(nix::libc::SIGINT, self.int);
            nix::libc::signal(nix::libc::SIGQUIT, self.quit);
        }
    }
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
    // When the last successful poll landed (distinct from the per-attempt
    // `refreshed` clock, which advances even on a failed fetch).
    let mut last_success: Option<Instant> = None;
    // Best-effort snapshot from the native `/api/status` route, merged for the
    // richer FC-link truth (port-open-but-silent). Absent → the gated boolean.
    let mut fc_status: Option<Value> = None;
    // `Some(i)` while the quick-actions overlay is open, with row `i` selected.
    let mut actions_selected: Option<usize> = None;
    // Trend buffers of verified telemetry, one sample per successful poll.
    let mut history = History::default();

    // One-shot background "newer version available?" check (pings GitHub via
    // curl off-thread; never blocks the loop). The launch splash is shown once,
    // then dismissed for the session; the footer badge stays as the reminder.
    let latest_slot = update::spawn_check();
    let mut update_splash = false;
    let mut update_splash_done = false;

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
                    last_success = Some(Instant::now());
                }
                Err(e) => error = Some(format!("Agent unreachable: {e}")),
            }
            // Best-effort: the native status route carries the FC transport /
            // heartbeat split. A failure here leaves the gated boolean standing.
            fc_status = client.status().ok();
            refreshed = now_hms();
            last_fetch = Some(Instant::now());
        }

        // The snapshot is stale when the last success is older than STALE_AFTER
        // (the fetch is erroring while an old snapshot is still on screen).
        let stale = last_success.is_some_and(|t| t.elapsed() > STALE_AFTER);
        let dash = data.as_ref().map(|v| {
            let mut d = Dashboard::from_status(v);
            if let Some(fc) = &fc_status {
                d.merge_fc_status(fc);
            }
            d
        });

        // The latest version from the background check (None until it lands).
        let latest = latest_slot.lock().ok().and_then(|g| g.clone());
        let update_available = matches!(
            (latest.as_deref(), dash.as_ref()),
            (Some(l), Some(d)) if update::is_newer(l, &d.version)
        );
        // Raise the launch splash once, the first time an update is seen (never
        // over the actions overlay).
        if update_available && !update_splash_done && actions_selected.is_none() {
            update_splash = true;
        }

        terminal.draw(|f| {
            ui::render(
                f,
                dash.as_ref(),
                &history,
                &refreshed,
                stale,
                error.as_deref(),
                actions_selected,
                latest.as_deref(),
                update_splash,
            )
        })?;

        if event::poll(TICK)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                // Ctrl-C always quits, from any screen.
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return Ok(());
                }
                // The launch update splash takes precedence: any key dismisses it
                // for the session, as its "any key: later" says. Only [u] asks for
                // the update, and that goes through the same y/N-confirmed action
                // as the dashboard key, refused while the vehicle reports armed.
                if update_splash {
                    update_splash = false;
                    update_splash_done = true;
                    if splash_requests_update(key.code) {
                        request_update(terminal, dash.as_ref().and_then(|d| d.armed))?;
                        last_fetch = None;
                    }
                    continue;
                }
                match actions_selected {
                    // The actions overlay is open.
                    Some(sel) => match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => actions_selected = None,
                        KeyCode::Up | KeyCode::Char('k') => {
                            actions_selected = Some(sel.saturating_sub(1));
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            actions_selected = Some((sel + 1).min(ACTIONS.len() - 1));
                        }
                        KeyCode::Enter => {
                            actions_selected = None;
                            let action = &ACTIONS[sel];
                            if action.args.first().copied() == Some("update") {
                                request_update(terminal, dash.as_ref().and_then(|d| d.armed))?;
                            } else {
                                run_action(terminal, action)?;
                            }
                            last_fetch = None; // refresh right after returning
                        }
                        _ => {}
                    },
                    // The dashboard.
                    None => match key.code {
                        KeyCode::Char('q') | KeyCode::Char('Q') => return Ok(()),
                        KeyCode::Char('a') | KeyCode::Char('A') => actions_selected = Some(0),
                        KeyCode::Char('r') | KeyCode::Char('R') => last_fetch = None,
                        // `[u] update` (shown in the footer only when a newer
                        // version is available) runs the agent update, y/N-gated.
                        KeyCode::Char('u') | KeyCode::Char('U') if update_available => {
                            request_update(terminal, dash.as_ref().and_then(|d| d.armed))?;
                            last_fetch = None;
                        }
                        KeyCode::Char(c) => {
                            let c = c.to_ascii_lowercase();
                            if let Some(action) = ACTIONS.iter().find(|a| a.key == Some(c)) {
                                run_action(terminal, action)?;
                                last_fetch = None;
                            }
                        }
                        _ => {}
                    },
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_u_on_the_update_splash_asks_for_the_update() {
        assert!(splash_requests_update(KeyCode::Char('u')));
        assert!(splash_requests_update(KeyCode::Char('U')));
        // Enter is the "any key: later" the splash advertises, not an update.
        assert!(!splash_requests_update(KeyCode::Enter));
        assert!(!splash_requests_update(KeyCode::Esc));
        assert!(!splash_requests_update(KeyCode::Char('q')));
    }
}
