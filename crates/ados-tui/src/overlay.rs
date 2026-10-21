//! Overlays: modal dialogs drawn over the screen, their state, and their key
//! handling. A screen opens one with a [`Ret`] naming itself and a tag; when
//! the overlay finishes, the app hands the [`OverlayResult`] back to that
//! screen.

use serde_json::Value;
use tui_input::Input;

use crate::keymap::Ctx;
use crate::palette::{PaletteAction, PaletteState};
use crate::toast::ToastKind;
use crate::worker::Route;

mod keys;

pub use keys::handle_key;

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
    pub fn typed_confirm(
        ret: Ret,
        title: impl Into<String>,
        body: Vec<String>,
        word: &'static str,
    ) -> Self {
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
    pub fn input(
        ret: Ret,
        title: impl Into<String>,
        prompt: impl Into<String>,
        initial: &str,
    ) -> InputState {
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
