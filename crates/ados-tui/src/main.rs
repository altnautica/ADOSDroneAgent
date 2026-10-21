//! ADOS Drone Agent terminal console.
//!
//! Launched by `ados` with no subcommand, or directly as `ados-tui` (with
//! `--host` to drive an agent on another machine). Tabs for the live overview,
//! vehicle, link, video, FC parameters, extensions, logs, system and settings;
//! every read and write goes through the agent's HTTP API.

mod action;
mod api_path;
mod app;
mod args;
mod clipboard;
mod ext;
mod keymap;
mod model;
mod motion;
mod overlay;
mod palette;
mod params;
mod poll;
mod prefs;
mod settings;
mod shell;
mod stream;
mod symbols;
mod theme;
mod toast;
mod ui;
mod update;
mod worker;

use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::crossterm::terminal::{enable_raw_mode, EnterAlternateScreen};
use ratatui::crossterm::ExecutableCommand;
use ratatui::Terminal;

use crate::action::{ArmView, ACTIONS};
use crate::app::{App, Core, ShellReq};
use crate::args::{Env, Parsed};
use crate::motion::MotionLevel;
use crate::prefs::PrefsStore;
use crate::shell::Term;

/// Idle input wait: the screen redraws on input, data, or this tick.
const IDLE_TICK: Duration = Duration::from_millis(250);
/// The header clock and toasts expire on whole seconds.
const CLOCK_TICK: Duration = Duration::from_secs(1);

fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match args::parse(&argv) {
        Parsed::Run(a) => a,
        Parsed::Help => {
            println!("{}", args::USAGE);
            return Ok(());
        }
        Parsed::Version => {
            println!("ados-tui {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Parsed::Error(e) => {
            eprintln!("ados-tui: {e}\n\n{}", args::USAGE);
            std::process::exit(2);
        }
    };
    let store = PrefsStore::load(prefs::env_path());
    let (target, key) = match args::resolve(&args, &Env::from_process(), &store.prefs.targets) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ados-tui: {e}");
            std::process::exit(2);
        }
    };

    shell::install_panic_hook();
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    shell::set_mouse(store.prefs.mouse);

    let latest = update::spawn_check();
    let mut app = App::new(Core::new(target, key, store, latest));
    let result = run(&mut terminal, &mut app);

    // Always restore the terminal on a clean exit too.
    shell::restore_terminal();
    let _ = terminal.show_cursor();
    result
}

fn run(terminal: &mut Term, app: &mut App) -> Result<()> {
    let mut last_clock = Instant::now();
    loop {
        app.pump();
        if let Some((target, key)) = app.core.retarget.take() {
            // Re-target in place: a fresh core (poller, worker, stream, state)
            // around the new agent, keeping the preferences and update check.
            let store = app.core.store.clone();
            let latest = app.core.latest.clone();
            let name = target.label();
            *app = App::new(Core::new(target, key, store, latest));
            app.core.toast(toast::ToastKind::Success, format!("Connected to {name}"));
        }
        if app.core.quit {
            return Ok(());
        }
        if let Some(req) = app.core.shell.take() {
            run_shell(terminal, app, req)?;
            app.core.dirty = true;
            continue;
        }
        if app.core.mouse_changed {
            app.core.mouse_changed = false;
            shell::set_mouse(app.core.store.prefs.mouse);
        }
        if last_clock.elapsed() >= CLOCK_TICK {
            last_clock = Instant::now();
            app.core.dirty = true;
        }
        if app.needs_frame() {
            // Effects run over the freshly drawn buffer in the same frame.
            terminal.draw(|f| {
                let out = ui::render(f, app);
                let area = f.area();
                app.post_render(out, f.buffer_mut(), area);
            })?;
        }
        let wait = if app.core.fx.running() {
            match app.core.fx.level {
                MotionLevel::Full => Duration::from_millis(33),
                _ => Duration::from_millis(80),
            }
        } else {
            IDLE_TICK
        };
        if event::poll(wait)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
                Event::Mouse(m) if app.core.store.prefs.mouse => app.on_mouse(m),
                Event::Resize(_, _) => app.core.dirty = true,
                _ => {}
            }
        }
    }
}

/// Run a shell-out the app asked for.
fn run_shell(terminal: &mut Term, app: &mut App, req: ShellReq) -> Result<()> {
    if app.core.target.remote {
        return Ok(());
    }
    let mouse = app.core.store.prefs.mouse;
    let arm = ArmView::from_snapshot(app.core.dash().and_then(|d| d.armed), app.core.stale());
    let profile = app.core.dash().map(|d| d.profile.clone());
    match req {
        ShellReq::Action(i) => {
            let Some(action) = ACTIONS.get(i) else { return Ok(()) };
            if action.is_update() {
                shell::request_update(terminal, arm, mouse)?;
            } else {
                shell::run_action(terminal, action, profile.as_deref(), None, mouse)?;
            }
        }
        ShellReq::Update => shell::request_update(terminal, arm, mouse)?,
        ShellReq::PluginInstall => {
            if let Some(path) = app.core.shell_arg.take() {
                let args = vec!["plugin".to_string(), "install".to_string(), path];
                shell::run_argv(terminal, "ados", &args, None, mouse)?;
            }
        }
    }
    app.core.poller.refresh();
    Ok(())
}
