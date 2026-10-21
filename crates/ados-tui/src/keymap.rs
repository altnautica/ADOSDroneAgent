//! The key map: every key binding, its command, its hint and its help text, in
//! one table. The hint bar and the help overlay are generated from this table,
//! so a key can never do something its hint does not say.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::TabId;

/// A key-binding context. Contexts stack: the most specific is tried first,
/// `Global` last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ctx {
    Global,
    /// Generic list navigation.
    List,
    /// The Settings tab (page and row navigation).
    Settings,
    Params,
    Logs,
    Extensions,
    /// A text field has focus: only Enter/Esc are commands, every other key is
    /// text.
    Input,
    /// A non-text overlay (picker, result, help, actions, notifications).
    Overlay,
    /// A yes/no confirmation.
    Confirm,
    /// A checklist overlay.
    Checklist,
    /// Keys of one settings page or tab body.
    Page(PageKeys),
}

/// Screens with their own action keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKeys {
    Targets,
    Profile,
    MacPin,
    Sensors,
    RemoteAccess,
    Wifi,
    Pairing,
    Mcp,
    Secret,
    System,
    Ground,
}

/// Every user command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    // global
    Tab(TabId),
    PrevTab,
    NextTab,
    Palette,
    Help,
    Search,
    SearchAll,
    Refresh,
    Copy,
    Actions,
    Update,
    Quit,
    ForceQuit,
    Back,
    // navigation
    Up,
    Down,
    PageUp,
    PageDown,
    Top,
    Bottom,
    Left,
    Right,
    Activate,
    NextPane,
    PrevPane,
    // text input and overlays
    Submit,
    Cancel,
    Yes,
    No,
    Toggle,
    // settings
    Reveal,
    // settings pages
    Add,
    Edit,
    Delete,
    Redetect,
    Pin,
    Unpin,
    LoadRig,
    PasteToken,
    Scan,
    Hidden,
    Disconnect,
    Forget,
    AcceptCode,
    Unpair,
    Mint,
    Revoke,
    RevokeAll,
    // params
    Unstage,
    DiscardAll,
    Favourite,
    StagedView,
    Export,
    Import,
    Write,
    // extensions
    Enable,
    Disable,
    Remove,
    Grant,
    RevokePerm,
    InstallUrl,
    InstallFile,
    // logs
    CycleLevel,
    PickService,
    Pause,
    Follow,
    // system
    RestartService,
    Recheck,
    RestartSupervisor,
    Reboot,
    // ground station
    SurveyIn,
    BaseOff,
    BtScan,
    Approve,
    Reject,
}

/// A key: its code and the modifiers that must be held. SHIFT is ignored for
/// character keys (the character already carries the case).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeySpec {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl KeySpec {
    pub const fn plain(code: KeyCode) -> Self {
        Self {
            code,
            mods: KeyModifiers::NONE,
        }
    }
    pub const fn ch(c: char) -> Self {
        Self::plain(KeyCode::Char(c))
    }
    pub const fn ctrl(c: char) -> Self {
        Self {
            code: KeyCode::Char(c),
            mods: KeyModifiers::CONTROL,
        }
    }

    /// Whether a pressed key matches this spec.
    pub fn matches(&self, key: &KeyEvent) -> bool {
        let relevant = KeyModifiers::CONTROL | KeyModifiers::ALT;
        let held = key.modifiers & relevant;
        let want = self.mods & relevant;
        match (self.code, key.code) {
            (KeyCode::Char(a), KeyCode::Char(b)) => a == b && held == want,
            (a, b) => a == b && held == want,
        }
    }
}

/// One binding row.
#[derive(Debug, Clone, Copy)]
pub struct Binding {
    pub ctx: Ctx,
    pub key: KeySpec,
    pub cmd: Command,
    /// Key text for the hint bar and help (e.g. `↑↓`, `^K`).
    pub label: &'static str,
    /// Short hint-bar word; `None` keeps the binding out of the hint bar.
    pub hint: Option<&'static str>,
    /// Help-overlay description.
    pub help: &'static str,
}

const fn b(
    ctx: Ctx,
    key: KeySpec,
    cmd: Command,
    label: &'static str,
    hint: Option<&'static str>,
    help: &'static str,
) -> Binding {
    Binding {
        ctx,
        key,
        cmd,
        label,
        hint,
        help,
    }
}

use Command as C;
use Ctx::*;
use KeyCode as K;
use PageKeys as P;

const fn kp(code: KeyCode) -> KeySpec {
    KeySpec::plain(code)
}
const fn ch(c: char) -> KeySpec {
    KeySpec::ch(c)
}

/// The whole key map.
#[rustfmt::skip]
pub const BINDINGS: &[Binding] = &[
    // --- text input ------------------------------------------------------
    b(Input, kp(K::Enter), C::Submit, "Enter", Some("ok"), "Submit"),
    b(Input, kp(K::Esc), C::Cancel, "Esc", Some("cancel"), "Cancel"),
    // --- confirm ---------------------------------------------------------
    b(Confirm, ch('y'), C::Yes, "y", Some("yes"), "Confirm"),
    b(Confirm, kp(K::Enter), C::Yes, "Enter", None, "Confirm"),
    b(Confirm, ch('n'), C::No, "n", Some("no"), "Cancel"),
    b(Confirm, kp(K::Esc), C::No, "Esc", None, "Cancel"),
    // --- checklist -------------------------------------------------------
    b(Checklist, ch(' '), C::Toggle, "Space", Some("toggle"), "Toggle the item"),
    // --- overlays ----------------------------------------------------------
    b(Overlay, kp(K::Up), C::Up, "↑↓", Some("move"), "Move"),
    b(Overlay, ch('k'), C::Up, "k", None, "Move up"),
    b(Overlay, kp(K::Down), C::Down, "↓", None, "Move down"),
    b(Overlay, ch('j'), C::Down, "j", None, "Move down"),
    b(Overlay, kp(K::PageUp), C::PageUp, "PgUp", None, "Page up"),
    b(Overlay, kp(K::PageDown), C::PageDown, "PgDn", None, "Page down"),
    b(Overlay, kp(K::Enter), C::Activate, "Enter", Some("select"), "Select"),
    b(Overlay, ch('y'), C::Copy, "y", None, "Copy"),
    b(Overlay, kp(K::Esc), C::Back, "Esc", Some("close"), "Close"),
    b(Overlay, ch('q'), C::Back, "q", None, "Close"),
    // --- params ------------------------------------------------------------
    b(Params, kp(K::Enter), C::Activate, "Enter", Some("edit"), "Edit the parameter"),
    b(Params, ch('w'), C::Write, "w", Some("write"), "Write staged values to the FC"),
    b(Params, ch('u'), C::Unstage, "u", Some("unstage"), "Unstage the row"),
    b(Params, ch('U'), C::DiscardAll, "U", None, "Discard every staged value"),
    b(Params, ch('f'), C::Favourite, "f", Some("fav"), "Toggle favourite"),
    b(Params, ch('m'), C::StagedView, "m", Some("staged"), "Show only staged values"),
    b(Params, ch('e'), C::Export, "e", Some("export"), "Export to a .param file"),
    b(Params, ch('i'), C::Import, "i", Some("import"), "Import a .param file"),
    // --- extensions --------------------------------------------------------
    b(Extensions, kp(K::Enter), C::Activate, "Enter", Some("install/edit"), "Install a catalog entry, or edit a config row"),
    b(Extensions, ch('e'), C::Enable, "e", Some("enable"), "Enable the extension"),
    b(Extensions, ch('d'), C::Disable, "d", Some("disable"), "Disable the extension"),
    b(Extensions, ch('x'), C::Remove, "x", Some("remove"), "Remove the extension"),
    b(Extensions, ch('g'), C::Grant, "g", Some("grant"), "Grant a missing permission"),
    b(Extensions, ch('G'), C::RevokePerm, "G", None, "Revoke a permission"),
    b(Extensions, ch('I'), C::InstallUrl, "I", Some("from URL"), "Install from a URL"),
    b(Extensions, ch('F'), C::InstallFile, "F", None, "Install a local .adosplug file"),
    // --- logs --------------------------------------------------------------
    b(Logs, ch('l'), C::CycleLevel, "l", Some("level"), "Cycle the minimum level"),
    b(Logs, ch('s'), C::PickService, "s", Some("service"), "Filter by service"),
    b(Logs, ch('p'), C::Pause, "p", Some("pause"), "Pause or resume the view"),
    b(Logs, ch('G'), C::Follow, "G", Some("follow"), "Follow the tail"),
    b(Logs, kp(K::End), C::Follow, "End", None, "Follow the tail"),
    b(Logs, kp(K::Enter), C::Activate, "Enter", Some("detail"), "Show the full line"),
    // --- settings ----------------------------------------------------------
    b(Settings, kp(K::Left), C::Left, "←→", Some("change"), "Change the value"),
    b(Settings, kp(K::Right), C::Right, "→", None, "Change the value"),
    b(Settings, kp(K::Enter), C::Activate, "Enter", Some("edit"), "Edit the value"),
    b(Settings, kp(K::Tab), C::NextPane, "Tab", Some("page"), "Next page"),
    b(Settings, kp(K::BackTab), C::PrevPane, "S-Tab", None, "Previous page"),
    b(Settings, ch('v'), C::Reveal, "v", None, "Reveal a secret for 10 s"),
    // --- settings pages ----------------------------------------------------
    b(Page(P::Targets), ch('n'), C::Add, "n", Some("add"), "Add a target"),
    b(Page(P::Targets), ch('e'), C::Edit, "e", Some("edit"), "Edit the target"),
    b(Page(P::Targets), ch('x'), C::Delete, "x", Some("delete"), "Delete the target"),
    b(Page(P::Targets), kp(K::Enter), C::Activate, "Enter", Some("connect"), "Connect to the target"),
    b(Page(P::Profile), ch('d'), C::Redetect, "d", Some("re-detect"), "Re-detect the profile"),
    b(Page(P::Profile), kp(K::Enter), C::Activate, "Enter", Some("apply"), "Apply the staged profile"),
    b(Page(P::MacPin), ch('p'), C::Pin, "p", Some("pin"), "Pin the adapter MAC"),
    b(Page(P::MacPin), ch('x'), C::Unpin, "x", Some("unpin"), "Unpin the adapter MAC"),
    b(Page(P::Sensors), ch('l'), C::LoadRig, "l", Some("load rig"), "Load a rig file"),
    b(Page(P::RemoteAccess), ch('t'), C::PasteToken, "t", Some("token"), "Paste a tunnel token"),
    b(Page(P::Wifi), ch('s'), C::Scan, "s", Some("scan"), "Scan for networks"),
    b(Page(P::Wifi), kp(K::Enter), C::Activate, "Enter", Some("join"), "Join the network"),
    b(Page(P::Wifi), ch('h'), C::Hidden, "h", Some("hidden"), "Join a hidden network"),
    b(Page(P::Wifi), ch('D'), C::Disconnect, "D", Some("disconnect"), "Disconnect"),
    b(Page(P::Wifi), ch('x'), C::Forget, "x", Some("forget"), "Forget the saved network"),
    b(Page(P::Pairing), ch('c'), C::AcceptCode, "c", Some("accept code"), "Accept a pairing code"),
    b(Page(P::Pairing), ch('U'), C::Unpair, "U", Some("unpair"), "Unpair this agent"),
    b(Page(P::Mcp), ch('n'), C::Mint, "n", Some("mint"), "Mint a token"),
    b(Page(P::Mcp), ch('x'), C::Revoke, "x", Some("revoke"), "Revoke the token"),
    b(Page(P::Mcp), ch('X'), C::RevokeAll, "X", None, "Revoke every token"),
    b(Page(P::Secret), ch('v'), C::Reveal, "v", Some("reveal"), "Reveal for 10 s"),
    // --- system ------------------------------------------------------------
    b(Page(P::System), ch('R'), C::RestartService, "R", Some("restart svc"), "Restart the service"),
    b(Page(P::System), ch('c'), C::Recheck, "c", Some("re-check"), "Re-run the hardware check"),
    b(Page(P::System), ch('S'), C::RestartSupervisor, "S", None, "Restart the supervisor"),
    b(Page(P::System), ch('X'), C::Reboot, "X", None, "Reboot the host"),
    // --- ground station ------------------------------------------------------
    b(Page(P::Ground), ch('b'), C::SurveyIn, "b", Some("survey-in"), "Start a base survey-in"),
    b(Page(P::Ground), ch('B'), C::BaseOff, "B", None, "Turn the base off"),
    b(Page(P::Ground), ch('s'), C::BtScan, "s", Some("bt scan"), "Scan for Bluetooth devices"),
    b(Page(P::Ground), ch('a'), C::Approve, "a", Some("approve"), "Approve the invite"),
    b(Page(P::Ground), ch('x'), C::Reject, "x", None, "Reject the invite"),
    b(Page(P::Ground), kp(K::Enter), C::Activate, "Enter", Some("select"), "Set primary / pair"),
    // --- lists -------------------------------------------------------------
    b(List, kp(K::Up), C::Up, "↑↓", Some("move"), "Move"),
    b(List, ch('k'), C::Up, "k", None, "Move up"),
    b(List, kp(K::Down), C::Down, "↓", None, "Move down"),
    b(List, ch('j'), C::Down, "j", None, "Move down"),
    b(List, kp(K::PageUp), C::PageUp, "PgUp", None, "Page up"),
    b(List, kp(K::PageDown), C::PageDown, "PgDn", None, "Page down"),
    b(List, kp(K::Home), C::Top, "Home", None, "First row"),
    b(List, ch('g'), C::Top, "g", None, "First row"),
    b(List, kp(K::End), C::Bottom, "End", None, "Last row"),
    b(List, ch('G'), C::Bottom, "G", None, "Last row"),
    b(List, kp(K::Left), C::Left, "←", None, "Left"),
    b(List, kp(K::Right), C::Right, "→", None, "Right"),
    b(List, kp(K::Enter), C::Activate, "Enter", None, "Open"),
    b(List, kp(K::Tab), C::NextPane, "Tab", Some("pane"), "Next pane"),
    b(List, kp(K::BackTab), C::PrevPane, "S-Tab", None, "Previous pane"),
    // --- global ------------------------------------------------------------
    b(Global, ch('1'), C::Tab(TabId::Overview), "1-9", None, "Go to tab"),
    b(Global, ch('2'), C::Tab(TabId::Vehicle), "2", None, "Go to tab 2"),
    b(Global, ch('3'), C::Tab(TabId::Link), "3", None, "Go to tab 3"),
    b(Global, ch('4'), C::Tab(TabId::Video), "4", None, "Go to tab 4"),
    b(Global, ch('5'), C::Tab(TabId::Params), "5", None, "Go to tab 5"),
    b(Global, ch('6'), C::Tab(TabId::Extensions), "6", None, "Go to tab 6"),
    b(Global, ch('7'), C::Tab(TabId::Logs), "7", None, "Go to tab 7"),
    b(Global, ch('8'), C::Tab(TabId::System), "8", None, "Go to tab 8"),
    b(Global, ch('9'), C::Tab(TabId::Settings), "9", None, "Go to tab 9"),
    b(Global, ch('['), C::PrevTab, "[", None, "Previous tab"),
    b(Global, ch(']'), C::NextTab, "]", None, "Next tab"),
    b(Global, ch('/'), C::Search, "/", Some("search"), "Search"),
    b(Global, KeySpec::ctrl('/'), C::SearchAll, "^/", None, "Search everything"),
    b(Global, KeySpec::ctrl('7'), C::SearchAll, "^/", None, "Search everything"),
    b(Global, KeySpec::ctrl('_'), C::SearchAll, "^/", None, "Search everything"),
    b(Global, KeySpec::ctrl('k'), C::Palette, "^K", Some("palette"), "Command palette"),
    b(Global, ch(':'), C::Palette, ":", None, "Command palette"),
    b(Global, ch('r'), C::Refresh, "r", None, "Refresh now"),
    b(Global, ch('y'), C::Copy, "y", None, "Copy the focused value"),
    b(Global, ch('a'), C::Actions, "a", Some("actions"), "Quick actions"),
    b(Global, ch('u'), C::Update, "u", Some("update"), "Update the agent"),
    b(Global, ch('?'), C::Help, "?", Some("help"), "Help"),
    b(Global, ch('q'), C::Quit, "q", Some("quit"), "Quit"),
    b(Global, KeySpec::ctrl('c'), C::ForceQuit, "^C", None, "Quit now"),
    b(Global, kp(K::Esc), C::Back, "Esc", None, "Close, clear, or go back"),
];

/// The command a key resolves to under a context stack (most specific
/// first). Under `Input` only Enter/Esc are commands; any other key is text,
/// and `None` is returned so the caller hands it to the focused field.
pub fn resolve(stack: &[Ctx], key: &KeyEvent) -> Option<Command> {
    for ctx in stack {
        if let Some(bind) = BINDINGS
            .iter()
            .find(|b| b.ctx == *ctx && b.key.matches(key))
        {
            return Some(bind.cmd);
        }
        if *ctx == Ctx::Input {
            return None;
        }
    }
    None
}

/// Hint-bar pairs `(key, word)` for a context stack, most specific first. A
/// command shadowed by a more specific context is not repeated, and `enabled`
/// drops commands that cannot run right now.
pub fn hints(
    stack: &[Ctx],
    enabled: impl Fn(Command) -> bool,
) -> Vec<(&'static str, &'static str)> {
    let mut out: Vec<(&'static str, &'static str)> = Vec::new();
    let mut seen_cmds: Vec<Command> = Vec::new();
    let mut seen_keys: Vec<KeySpec> = Vec::new();
    for ctx in stack {
        for bind in BINDINGS.iter().filter(|b| b.ctx == *ctx) {
            let shadowed = seen_keys.contains(&bind.key);
            seen_keys.push(bind.key);
            let Some(word) = bind.hint else { continue };
            if shadowed || seen_cmds.contains(&bind.cmd) || !enabled(bind.cmd) {
                continue;
            }
            seen_cmds.push(bind.cmd);
            out.push((bind.label, word));
        }
        if *ctx == Ctx::Input {
            break;
        }
    }
    out
}

/// Help-overlay rows `(key, description)` for a context stack, grouped by
/// context, most specific first.
pub fn help(stack: &[Ctx]) -> Vec<(&'static str, &'static str)> {
    let mut out = Vec::new();
    let mut seen_keys: Vec<KeySpec> = Vec::new();
    for ctx in stack {
        for bind in BINDINGS.iter().filter(|b| b.ctx == *ctx) {
            if seen_keys.contains(&bind.key) {
                continue;
            }
            seen_keys.push(bind.key);
            // Digit rows 2-9 collapse into the single "1-9" row, and chords
            // that terminals report differently (Ctrl-/ arrives as Ctrl-7 or
            // Ctrl-_) share one row.
            if matches!(bind.cmd, Command::Tab(t) if t != TabId::Overview)
                || out.contains(&(bind.label, bind.help))
            {
                continue;
            }
            out.push((bind.label, bind.help));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEventKind;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: ratatui::crossterm::event::KeyEventState::NONE,
        }
    }

    #[test]
    fn the_most_specific_context_wins() {
        let u = key(KeyCode::Char('u'), KeyModifiers::NONE);
        assert_eq!(
            resolve(&[Ctx::Params, Ctx::List, Ctx::Global], &u),
            Some(Command::Unstage)
        );
        assert_eq!(
            resolve(&[Ctx::List, Ctx::Global], &u),
            Some(Command::Update)
        );
        let shift_g = key(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(
            resolve(&[Ctx::Logs, Ctx::List, Ctx::Global], &shift_g),
            Some(Command::Follow)
        );
        assert_eq!(
            resolve(&[Ctx::List, Ctx::Global], &shift_g),
            Some(Command::Bottom)
        );
    }

    #[test]
    fn input_swallows_text_but_not_enter() {
        let stack = [Ctx::Input, Ctx::Global];
        assert_eq!(
            resolve(&stack, &key(KeyCode::Char('q'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            resolve(&stack, &key(KeyCode::Char('3'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            resolve(&stack, &key(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Command::Submit)
        );
    }

    #[test]
    fn control_chords_need_control() {
        let stack = [Ctx::Global];
        assert_eq!(
            resolve(&stack, &key(KeyCode::Char('k'), KeyModifiers::CONTROL)),
            Some(Command::Palette)
        );
        assert_eq!(
            resolve(&stack, &key(KeyCode::Char('k'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            resolve(&stack, &key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Command::ForceQuit)
        );
    }

    #[test]
    fn hints_skip_shadowed_and_disabled_commands() {
        let stack = [Ctx::Params, Ctx::List, Ctx::Global];
        let h = hints(&stack, |c| c != Command::Actions);
        assert!(h.contains(&("u", "unstage")));
        assert!(!h.iter().any(|(_, w)| *w == "update"), "{h:?}");
        assert!(!h.iter().any(|(_, w)| *w == "actions"), "{h:?}");
        assert!(h.contains(&("q", "quit")));
    }

    #[test]
    fn help_lists_each_row_once() {
        let rows = help(&[Ctx::Global]);
        let search_all = rows.iter().filter(|r| r.1 == "Search everything").count();
        assert_eq!(search_all, 1, "{rows:?}");
        assert!(rows.contains(&("1-9", "Go to tab")));
        assert!(!rows.iter().any(|(k, _)| *k == "2"), "{rows:?}");
    }
}
