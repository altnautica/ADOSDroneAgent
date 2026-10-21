//! Overlays: modal dialogs drawn over the screen, their state, and their key
//! handling. A screen opens one with a [`Ret`] naming itself and a tag; when
//! the overlay finishes, the app hands the [`OverlayResult`] back to that
//! screen.

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent};
use serde_json::Value;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use crate::keymap::{self, Command, Ctx};
use crate::palette::{PaletteAction, PaletteState};
use crate::toast::ToastKind;
use crate::worker::Route;

/// Where an overlay result goes, and what it was for.
#[derive(Debug, Clone, PartialEq)]
pub struct Ret {
    pub route: Route,
    /// The screen's own name for the question asked.
    pub tag: &'static str,
    /// Anything the screen wants back with the answer.
    pub data: Value,
}

impl Ret {
    pub fn new(route: Route, tag: &'static str) -> Self {
        Self {
            route,
            tag,
            data: Value::Null,
        }
    }
    pub fn with(mut self, data: Value) -> Self {
        self.data = data;
        self
    }
}

/// How an overlay finished.
#[derive(Debug, Clone, PartialEq)]
pub enum OverlayResult {
    Confirmed,
    Cancelled,
    Text(String),
    /// A nullable field was cleared (Backspace on an empty input).
    Null,
    Picked(usize),
    Checked(Vec<bool>),
}

/// A yes/no (or typed) confirmation.
#[derive(Debug, Clone)]
pub struct ConfirmState {
    pub title: String,
    pub body: Vec<String>,
    /// When set, the operator must type this word to confirm.
    pub typed: Option<&'static str>,
    pub input: Input,
    pub danger: bool,
    pub ret: Ret,
}

/// Input validation run on submit; `Err` keeps the overlay open.
pub type Validate = fn(&str) -> Result<(), String>;

/// A one-line text prompt.
#[derive(Debug, Clone)]
pub struct InputState {
    pub title: String,
    pub prompt: String,
    pub input: Input,
    pub masked: bool,
    /// Backspace on an empty field returns [`OverlayResult::Null`].
    pub nullable: bool,
    pub validate: Option<Validate>,
    pub error: Option<String>,
    /// A dim line under the field.
    pub hint: Option<String>,
    /// Draw the field on the focused settings row instead of in a dialog.
    pub inline: bool,
    pub ret: Ret,
}

/// A single-choice list.
#[derive(Debug, Clone)]
pub struct PickerState {
    pub title: String,
    pub items: Vec<String>,
    pub selected: usize,
    pub ret: Ret,
}

/// One checklist row.
#[derive(Debug, Clone)]
pub struct CheckItem {
    pub label: String,
    pub checked: bool,
    /// Shown but not toggleable (a required permission).
    pub locked: bool,
    /// Bit value, for a bitmask checklist that shows the decimal sum.
    pub bit: Option<u64>,
}

/// A multi-choice list.
#[derive(Debug, Clone)]
pub struct ChecklistState {
    pub title: String,
    pub items: Vec<CheckItem>,
    pub selected: usize,
    pub footer: Option<String>,
    pub ret: Ret,
}

impl ChecklistState {
    /// The decimal sum of the checked bits.
    pub fn bit_sum(&self) -> u64 {
        self.items
            .iter()
            .filter(|i| i.checked)
            .filter_map(|i| i.bit)
            .sum()
    }
}

/// A read-only result or detail view.
#[derive(Debug, Clone)]
pub struct ResultState {
    pub title: String,
    pub lines: Vec<(ToastKind, String)>,
    pub scroll: usize,
    /// What `y` copies, when anything.
    pub copy: Option<String>,
}

/// A secret shown once (an MCP token).
#[derive(Debug, Clone)]
pub struct SecretState {
    pub title: String,
    pub secret: String,
    pub note: String,
}

/// The API-key / PIN prompt for a remote agent.
#[derive(Debug, Clone)]
pub struct AuthState {
    pub mode: AuthMode,
    pub input: Input,
    pub message: Option<String>,
    /// The key was accepted and the operator is asked whether to save it.
    pub offer_remember: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    Key,
    Pin,
    /// No PIN is set or the node is locked: a message only.
    Blocked,
}

/// A modal overlay.
#[derive(Debug, Clone)]
pub enum Overlay {
    Help,
    Palette(Box<PaletteState>),
    Confirm(ConfirmState),
    Input(InputState),
    Picker(PickerState),
    Checklist(ChecklistState),
    Result(ResultState),
    /// The quick-actions list, with the selected row.
    Actions(usize),
    UpdateSplash,
    Auth(AuthState),
    Secret(SecretState),
    /// The notification history, scrolled back by `n` rows.
    Notifications(usize),
}

impl Overlay {
    /// Build a yes/no confirmation.
    pub fn confirm(ret: Ret, title: impl Into<String>, body: Vec<String>) -> Self {
        Overlay::Confirm(ConfirmState {
            title: title.into(),
            body,
            typed: None,
            input: Input::default(),
            danger: false,
            ret,
        })
    }

    /// Build a confirmation the operator confirms by typing `word`.
    pub fn typed_confirm(ret: Ret, title: impl Into<String>, body: Vec<String>, word: &'static str) -> Self {
        Overlay::Confirm(ConfirmState {
            title: title.into(),
            body,
            typed: Some(word),
            input: Input::default(),
            danger: true,
            ret,
        })
    }

    /// Build a text prompt.
    pub fn input(ret: Ret, title: impl Into<String>, prompt: impl Into<String>, initial: &str) -> InputState {
        InputState {
            title: title.into(),
            prompt: prompt.into(),
            input: Input::new(initial.to_string()),
            masked: false,
            nullable: false,
            validate: None,
            error: None,
            hint: None,
            inline: false,
            ret,
        }
    }

    /// Build a picker with `selected` preselected.
    pub fn picker(ret: Ret, title: impl Into<String>, items: Vec<String>, selected: usize) -> Self {
        Overlay::Picker(PickerState {
            title: title.into(),
            items,
            selected,
            ret,
        })
    }

    /// Build a result view.
    pub fn result(title: impl Into<String>, lines: Vec<(ToastKind, String)>) -> Self {
        Overlay::Result(ResultState {
            title: title.into(),
            lines,
            scroll: 0,
            copy: None,
        })
    }

    /// Key contexts while this overlay is on top.
    pub fn ctx(&self) -> Vec<Ctx> {
        match self {
            Overlay::Input(_) | Overlay::Palette(_) | Overlay::Auth(_) => vec![Ctx::Input],
            Overlay::Confirm(c) if c.typed.is_some() => vec![Ctx::Input],
            Overlay::Confirm(_) => vec![Ctx::Confirm],
            Overlay::Checklist(_) => vec![Ctx::Checklist, Ctx::Overlay],
            _ => vec![Ctx::Overlay],
        }
    }
}

/// What a key on the top overlay asks the app to do.
#[derive(Debug)]
pub enum Outcome {
    /// Nothing for the app (the overlay updated itself).
    None,
    /// Close the overlay without a result.
    Close,
    /// Close and deliver the result to the opener.
    Done(Ret, OverlayResult),
    /// Close and run a palette entry.
    Palette(PaletteAction),
    /// Close and run quick action `i`.
    Action(usize),
    /// Close and run the agent update.
    Update,
    /// Copy text (the overlay stays).
    Copy(String),
    /// Auth prompt submitted.
    AuthSubmit(String),
    /// Auth "remember the key" answered.
    AuthRemember(bool),
}

fn feed(input: &mut Input, key: KeyEvent) {
    input.handle_event(&Event::Key(key));
}

fn step(sel: usize, len: usize, down: bool) -> usize {
    if len == 0 {
        0
    } else if down {
        (sel + 1).min(len - 1)
    } else {
        sel.saturating_sub(1)
    }
}

/// Handle a key on the top overlay. `actions_len` is the quick-action count.
pub fn handle_key(overlay: &mut Overlay, key: KeyEvent, actions_len: usize, history_len: usize) -> Outcome {
    let cmd = keymap::resolve(&overlay.ctx(), &key);
    match overlay {
        Overlay::UpdateSplash => {
            if matches!(key.code, KeyCode::Char('u') | KeyCode::Char('U')) {
                Outcome::Update
            } else {
                Outcome::Close
            }
        }
        Overlay::Help => match cmd {
            Some(Command::Back) | Some(Command::Activate) => Outcome::Close,
            _ if key.code == KeyCode::Char('?') => Outcome::Close,
            _ => Outcome::None,
        },
        Overlay::Palette(p) => match key.code {
            KeyCode::Up => {
                p.selected = p.selected.saturating_sub(1);
                Outcome::None
            }
            KeyCode::Down => {
                p.selected = step(p.selected, p.ranked.len(), true);
                Outcome::None
            }
            _ => match cmd {
                Some(Command::Cancel) => Outcome::Close,
                Some(Command::Submit) => match p.chosen() {
                    Some(a) => Outcome::Palette(a),
                    None => Outcome::None,
                },
                _ => {
                    feed(&mut p.input, key);
                    p.rerank();
                    Outcome::None
                }
            },
        },
        Overlay::Confirm(c) => match (c.typed, cmd) {
            (Some(word), Some(Command::Submit)) => {
                if c.input.value().trim() == word {
                    Outcome::Done(c.ret.clone(), OverlayResult::Confirmed)
                } else {
                    Outcome::None
                }
            }
            (Some(_), Some(Command::Cancel)) | (None, Some(Command::No)) => {
                Outcome::Done(c.ret.clone(), OverlayResult::Cancelled)
            }
            (None, Some(Command::Yes)) => Outcome::Done(c.ret.clone(), OverlayResult::Confirmed),
            (Some(_), None) => {
                feed(&mut c.input, key);
                Outcome::None
            }
            _ => Outcome::None,
        },
        Overlay::Input(s) => match cmd {
            Some(Command::Cancel) => Outcome::Done(s.ret.clone(), OverlayResult::Cancelled),
            Some(Command::Submit) => {
                let value = s.input.value().to_string();
                if let Some(check) = s.validate {
                    if let Err(e) = check(&value) {
                        s.error = Some(e);
                        return Outcome::None;
                    }
                }
                Outcome::Done(s.ret.clone(), OverlayResult::Text(value))
            }
            _ => {
                if s.nullable && key.code == KeyCode::Backspace && s.input.value().is_empty() {
                    return Outcome::Done(s.ret.clone(), OverlayResult::Null);
                }
                feed(&mut s.input, key);
                s.error = None;
                Outcome::None
            }
        },
        Overlay::Picker(p) => match cmd {
            Some(Command::Up) => {
                p.selected = step(p.selected, p.items.len(), false);
                Outcome::None
            }
            Some(Command::Down) => {
                p.selected = step(p.selected, p.items.len(), true);
                Outcome::None
            }
            Some(Command::PageUp) => {
                p.selected = p.selected.saturating_sub(10);
                Outcome::None
            }
            Some(Command::PageDown) => {
                p.selected = (p.selected + 10).min(p.items.len().saturating_sub(1));
                Outcome::None
            }
            Some(Command::Activate) if !p.items.is_empty() => {
                Outcome::Done(p.ret.clone(), OverlayResult::Picked(p.selected))
            }
            Some(Command::Back) => Outcome::Done(p.ret.clone(), OverlayResult::Cancelled),
            _ => Outcome::None,
        },
        Overlay::Checklist(c) => match cmd {
            Some(Command::Up) => {
                c.selected = step(c.selected, c.items.len(), false);
                Outcome::None
            }
            Some(Command::Down) => {
                c.selected = step(c.selected, c.items.len(), true);
                Outcome::None
            }
            Some(Command::Toggle) => {
                if let Some(item) = c.items.get_mut(c.selected) {
                    if !item.locked {
                        item.checked = !item.checked;
                    }
                }
                Outcome::None
            }
            Some(Command::Activate) => Outcome::Done(
                c.ret.clone(),
                OverlayResult::Checked(c.items.iter().map(|i| i.checked).collect()),
            ),
            Some(Command::Back) => Outcome::Done(c.ret.clone(), OverlayResult::Cancelled),
            _ => Outcome::None,
        },
        Overlay::Result(r) => match cmd {
            Some(Command::Up) => {
                r.scroll = r.scroll.saturating_sub(1);
                Outcome::None
            }
            Some(Command::Down) => {
                r.scroll = (r.scroll + 1).min(r.lines.len().saturating_sub(1));
                Outcome::None
            }
            Some(Command::PageUp) => {
                r.scroll = r.scroll.saturating_sub(10);
                Outcome::None
            }
            Some(Command::PageDown) => {
                r.scroll = (r.scroll + 10).min(r.lines.len().saturating_sub(1));
                Outcome::None
            }
            Some(Command::Copy) => match &r.copy {
                Some(text) => Outcome::Copy(text.clone()),
                None => Outcome::None,
            },
            Some(Command::Back) | Some(Command::Activate) => Outcome::Close,
            _ => Outcome::None,
        },
        Overlay::Secret(s) => match cmd {
            Some(Command::Copy) => Outcome::Copy(s.secret.clone()),
            Some(Command::Back) => Outcome::Close,
            _ => Outcome::None,
        },
        Overlay::Actions(sel) => match cmd {
            Some(Command::Up) => {
                *sel = step(*sel, actions_len, false);
                Outcome::None
            }
            Some(Command::Down) => {
                *sel = step(*sel, actions_len, true);
                Outcome::None
            }
            Some(Command::Activate) => Outcome::Action(*sel),
            Some(Command::Back) => Outcome::Close,
            _ => Outcome::None,
        },
        Overlay::Notifications(back) => match cmd {
            Some(Command::Up) => {
                *back = (*back + 1).min(history_len.saturating_sub(1));
                Outcome::None
            }
            Some(Command::Down) => {
                *back = back.saturating_sub(1);
                Outcome::None
            }
            Some(Command::Back) | Some(Command::Activate) => Outcome::Close,
            _ => Outcome::None,
        },
        Overlay::Auth(a) => {
            if a.offer_remember {
                return match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => Outcome::AuthRemember(true),
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Enter => {
                        Outcome::AuthRemember(false)
                    }
                    _ => Outcome::None,
                };
            }
            match cmd {
                Some(Command::Submit) if a.mode != AuthMode::Blocked => {
                    Outcome::AuthSubmit(a.input.value().to_string())
                }
                // Esc closes the prompt; the shell stays usable and data
                // panels read "Locked" until a key or PIN works.
                Some(Command::Cancel) => Outcome::Close,
                _ => {
                    if a.mode != AuthMode::Blocked {
                        feed(&mut a.input, key);
                        a.message = None;
                    }
                    Outcome::None
                }
            }
        }
    }
}
