//! Terminal setup/teardown and the on-box shell-outs (quick actions, the agent
//! update, a local plugin install). A shell-out leaves the alternate screen so
//! the command's own output, and any sudo prompt or its own confirmation, is
//! visible, then restores the UI.

use std::io::{Stdout, Write};

use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::crossterm::{execute, ExecutableCommand};
use ratatui::Terminal;

use crate::action::{confirm_prompt, update_request, Action, ArmView};

pub type Term = Terminal<CrosstermBackend<Stdout>>;

/// Restore the terminal out of raw mode and the alternate screen.
pub fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), DisableMouseCapture, LeaveAlternateScreen, Show);
}

/// Restore the terminal from a panic hook before the default hook runs, so a
/// panic in the render loop never strands the operator's shell in raw mode +
/// the alternate screen. The release profile aborts on panic, but a hook still
/// runs before the abort.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default(info);
    }));
}

/// Turn mouse capture on or off.
pub fn set_mouse(on: bool) {
    let mut out = std::io::stdout();
    let _ = if on {
        execute!(out, EnableMouseCapture)
    } else {
        execute!(out, DisableMouseCapture)
    };
}

/// Put the UI back after a shell-out: raw mode, alt screen, mouse, full redraw.
fn restore_ui(terminal: &mut Term, mouse: bool) -> Result<()> {
    enable_raw_mode()?;
    std::io::stdout().execute(EnterAlternateScreen)?;
    set_mouse(mouse);
    terminal.clear()?;
    Ok(())
}

fn read_line() -> String {
    let mut buf = String::new();
    let _ = std::io::stdin().read_line(&mut buf);
    buf
}

/// Hold the plain terminal until the operator presses Enter, then restore.
fn pause_then_restore(terminal: &mut Term, mouse: bool) -> Result<()> {
    print!("\nPress Enter to return to the dashboard… ");
    let _ = std::io::stdout().flush();
    let _ = read_line();
    restore_ui(terminal, mouse)
}

/// Show why a request was refused on the plain terminal, in the same frame an
/// action's own output would appear in, so the refusal is not a silent no-op.
fn show_refusal(terminal: &mut Terminal<CrosstermBackend<Stdout>>, why: &str, mouse: bool) -> Result<()> {
    restore_terminal();
    println!("\n{why}");
    pause_then_restore(terminal, mouse)
}

/// Run an update request: the confirming update action (its prompt naming an
/// unknown arm state), or the refusal while the vehicle reports armed.
pub fn request_update(terminal: &mut Term, arm: ArmView, mouse: bool) -> Result<()> {
    match update_request(arm) {
        Ok(action) => run_action(terminal, action, None, Some(arm), mouse),
        Err(why) => show_refusal(terminal, why, mouse),
    }
}

/// Run a quick action by shelling out to the real terminal.
pub fn run_action(
    terminal: &mut Term,
    action: &Action,
    profile: Option<&str>,
    arm: Option<ArmView>,
    mouse: bool,
) -> Result<()> {
    let (program, args) = match action.argv(profile) {
        Ok(argv) => argv,
        Err(why) => return show_refusal(terminal, why, mouse),
    };
    let args: Vec<String> = args.into_iter().map(str::to_string).collect();
    let prompt = action.confirm.then(|| confirm_prompt(action.label, arm));
    run_argv(terminal, program, &args, prompt.as_deref(), mouse)
}

/// Run `program args…` on the plain terminal, optionally y/N-confirmed first.
pub fn run_argv(
    terminal: &mut Term,
    program: &str,
    args: &[String],
    confirm: Option<&str>,
    mouse: bool,
) -> Result<()> {
    restore_terminal();
    // The terminal is now in cooked mode (Ctrl-C raises SIGINT). Ignore SIGINT +
    // SIGQUIT in this process for the duration of the action so a Ctrl-C used to
    // stop a streaming child exits the child, not the whole UI. `spawn_action`
    // resets them to default in the child so it still receives them. Restored
    // on return (RAII).
    #[cfg(unix)]
    let _signals = SignalGuard::ignore();

    if let Some(prompt) = confirm {
        print!("\n{prompt}");
        let _ = std::io::stdout().flush();
        if !read_line().trim().eq_ignore_ascii_case("y") {
            return restore_ui(terminal, mouse);
        }
    }

    println!("\n$ {program} {}\n", args.join(" "));
    match spawn_action(program, args) {
        Ok(status) if !status.success() => {
            println!("\n[exited with status {}]", status.code().unwrap_or(-1));
        }
        Err(e) => println!("\n[could not run {program}: {e}]"),
        _ => {}
    }
    pause_then_restore(terminal, mouse)
}

/// Spawn a child inheriting this terminal, resetting SIGINT/SIGQUIT to their
/// default disposition in the child (they are ignored in the parent for the
/// child's duration) so a Ctrl-C reaches the child but not this UI.
fn spawn_action(program: &str, args: &[String]) -> std::io::Result<std::process::ExitStatus> {
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
