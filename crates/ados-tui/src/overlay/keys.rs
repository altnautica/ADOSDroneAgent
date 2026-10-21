//! Key handling for the top overlay.

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent};
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use super::{AuthMode, Outcome, Overlay, OverlayResult};
use crate::keymap::{self, Command};

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
pub fn handle_key(
    overlay: &mut Overlay,
    key: KeyEvent,
    actions_len: usize,
    history_len: usize,
) -> Outcome {
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
