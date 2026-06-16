//! Generic GPIO-output substrate: drive a status buzzer or LED on a host GPIO
//! line, and compute a software-PWM beep schedule.
//!
//! This is the agent's first hardware-OUTPUT crate (the front-panel button
//! reader in `ados-hid` is the GPIO-INPUT precedent). It is split the same way:
//!
//! * The hardware-coupled half ([`GpioOutput`]) opens a `/dev/gpiochip*` line in
//!   OUTPUT mode and drives it high/low. It is `#[cfg(target_os = "linux")]` over
//!   `gpio-cdev` (the same pure-Rust character-device backend the button reader
//!   uses, no libgpiod C dependency), so the rest of the crate still builds and
//!   tests on a non-Linux dev host.
//! * The host-portable half ([`BeepPattern`] + [`beep_schedule`] + [`Command`])
//!   is pure timing math and command parsing, fully unit-tested without any
//!   hardware. A software-PWM beep is a list of toggle instants the service can
//!   apply to a [`GpioOutput`]; computing it is pure.
//!
//! Safe-by-default: a fresh service drives NO line until it receives an explicit
//! command, and every beep pattern is bounded ([`BeepPattern::clamp`]) so a
//! command can never hold a line high indefinitely or schedule an unbounded run.

use serde::{Deserialize, Serialize};

pub mod sidecar;

#[cfg(target_os = "linux")]
mod linux_output;
#[cfg(target_os = "linux")]
pub use linux_output::GpioOutput;

/// Operator command socket the service serves and the plugin host forwards to
/// (`/run/ados/gpio-cmd.sock`). Mirrors the radio's `radio-cmd.sock` and the
/// net manager's `wifi-cmd.sock` pattern: one newline-JSON request, one
/// newline-JSON response per connection.
pub const GPIO_CMD_SOCK: &str = "/run/ados/gpio-cmd.sock";

// ---------------------------------------------------------------------------
// Bounds. A command is always clamped into these before it can reach hardware,
// so a malformed or hostile request can never hold a line high indefinitely.
// ---------------------------------------------------------------------------

/// Largest number of on/off cycles a single beep pattern may schedule. A request
/// for more is clamped down to this.
pub const MAX_BEEP_CYCLES: u32 = 64;

/// Longest a single on or off phase may last, in milliseconds. Caps the worst
/// case where one cycle of a clamped pattern still holds the line for a long
/// time.
pub const MAX_PHASE_MS: u32 = 5_000;

/// Hard ceiling on the total scheduled duration of one pattern, in milliseconds
/// (30 s). Even a pattern within the per-phase and per-cycle bounds is truncated
/// so the whole beep can never run away; the service stops driving and returns
/// the line low once the schedule completes.
pub const MAX_TOTAL_MS: u64 = 30_000;

/// The logical level a pin is driven to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Drive the line low (0).
    Low,
    /// Drive the line high (1).
    High,
}

impl Level {
    /// The 0/1 line value this level writes.
    pub fn value(self) -> u8 {
        match self {
            Level::Low => 0,
            Level::High => 1,
        }
    }
}

// ---------------------------------------------------------------------------
// Beep pattern (software PWM) — pure timing model.
// ---------------------------------------------------------------------------

/// A bounded software-PWM beep description.
///
/// `freq_hz` and `duty_pct` describe the carrier a passive buzzer wants; for a
/// simple active buzzer or an LED the schedule just toggles on `on_ms` then off
/// `off_ms` for `cycles`. The carrier is modelled too so a passive buzzer driver
/// can derive the toggle period from `freq_hz`; [`beep_schedule`] emits the
/// envelope (the on/off cycles), which is what the service applies.
///
/// Every field is advisory until [`clamp`](Self::clamp) bounds it; the service
/// always clamps before scheduling, so a value out of range is corrected, never
/// trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeepPattern {
    /// Buzzer carrier frequency in Hz (0 = no carrier, a plain on/off envelope
    /// for an active buzzer or LED). Advisory; consumed by a passive-buzzer
    /// driver, not by the envelope schedule.
    pub freq_hz: u32,
    /// Carrier duty cycle as a percentage (0..=100). Clamped to 100.
    pub duty_pct: u8,
    /// Milliseconds the line is held high per cycle.
    pub on_ms: u32,
    /// Milliseconds the line is held low between cycles.
    pub off_ms: u32,
    /// Number of on/off cycles to run.
    pub cycles: u32,
}

impl BeepPattern {
    /// Clamp every field into the safe bounds. `duty_pct` is capped at 100,
    /// `on_ms`/`off_ms` at [`MAX_PHASE_MS`], `cycles` at [`MAX_BEEP_CYCLES`].
    /// Idempotent: clamping an already-clamped pattern is a no-op.
    pub fn clamp(self) -> Self {
        Self {
            freq_hz: self.freq_hz,
            duty_pct: self.duty_pct.min(100),
            on_ms: self.on_ms.min(MAX_PHASE_MS),
            off_ms: self.off_ms.min(MAX_PHASE_MS),
            cycles: self.cycles.min(MAX_BEEP_CYCLES),
        }
    }

    /// Total scheduled duration of the clamped pattern, in milliseconds, before
    /// the [`MAX_TOTAL_MS`] truncation. Used by [`beep_schedule`] to truncate.
    fn raw_total_ms(self) -> u64 {
        let per_cycle = self.on_ms as u64 + self.off_ms as u64;
        per_cycle.saturating_mul(self.cycles as u64)
    }
}

/// One phase of a beep schedule: hold the line at `level` for `hold_ms`, then
/// move to the next phase. The final phase always returns the line low.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BeepPhase {
    /// The level to drive for this phase.
    pub level: Level,
    /// How long to hold it, in milliseconds.
    pub hold_ms: u32,
}

/// Compute the toggle schedule for a beep pattern. Pure: no hardware, no clock,
/// fully unit-testable. The pattern is clamped first, then expanded into a list
/// of [`BeepPhase`]s (a `High` for `on_ms` then a `Low` for `off_ms`, repeated
/// `cycles` times), and the whole sequence is truncated so its summed duration
/// never exceeds [`MAX_TOTAL_MS`]. The returned schedule always ends with the
/// line low, so the service never leaves a buzzer/LED stuck on.
///
/// A zero-cycle (or all-zero-duration) pattern yields a single terminal `Low`
/// phase of 0 ms — the service drives the line low and is done.
pub fn beep_schedule(pattern: BeepPattern) -> Vec<BeepPhase> {
    let p = pattern.clamp();
    let mut phases = Vec::new();
    let mut elapsed: u64 = 0;

    // The truncation budget: never schedule past MAX_TOTAL_MS regardless of how
    // the clamped per-phase/per-cycle bounds multiply out.
    let budget = p.raw_total_ms().min(MAX_TOTAL_MS);

    'outer: for _ in 0..p.cycles {
        for (level, ms) in [(Level::High, p.on_ms), (Level::Low, p.off_ms)] {
            if ms == 0 {
                continue;
            }
            let remaining = budget.saturating_sub(elapsed);
            if remaining == 0 {
                break 'outer;
            }
            let hold = (ms as u64).min(remaining) as u32;
            phases.push(BeepPhase {
                level,
                hold_ms: hold,
            });
            elapsed += hold as u64;
        }
    }

    // Always finish low so a buzzer/LED is never left energized. If the last
    // emitted phase is already Low we still append a 0 ms Low terminator so the
    // contract ("the schedule ends low") holds without inspecting the tail.
    phases.push(BeepPhase {
        level: Level::Low,
        hold_ms: 0,
    });
    phases
}

// ---------------------------------------------------------------------------
// Command schema — the wire protocol the command socket accepts. Pure parsing.
// ---------------------------------------------------------------------------

/// A validated GPIO-output command, parsed from one request line. Parsing is
/// pure (no hardware), so every malformed request is rejected before the service
/// touches a line. A `set` drives one line to a level; a `beep` runs a bounded
/// software-PWM envelope on one line; `status` reports the current line states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Drive `pin` on chip `chip` to `level`.
    Set { chip: u32, pin: u32, level: Level },
    /// Run a bounded beep envelope on `pin` of chip `chip`.
    Beep {
        chip: u32,
        pin: u32,
        pattern: BeepPattern,
    },
    /// Report the current driven line states.
    Status,
}

/// The raw request wire shape. `op` selects the command; the remaining fields
/// are optional and validated per op. A `default` chip of 0 matches the common
/// single-controller board where every line is on `/dev/gpiochip0`.
#[derive(Debug, Deserialize)]
struct Request {
    op: String,
    #[serde(default)]
    chip: Option<u32>,
    #[serde(default)]
    pin: Option<u32>,
    #[serde(default)]
    level: Option<Level>,
    #[serde(default)]
    freq_hz: Option<u32>,
    #[serde(default)]
    duty_pct: Option<u8>,
    #[serde(default)]
    on_ms: Option<u32>,
    #[serde(default)]
    off_ms: Option<u32>,
    #[serde(default)]
    cycles: Option<u32>,
}

/// The outcome of parsing a request line: a routed [`Command`], or a terminal
/// machine-readable error string for a malformed/unknown request (answered
/// without ever touching hardware).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// A validated, apply-ready command.
    Cmd(Command),
    /// A stable error code the caller renders as `{"ok":false,"error":<code>}`.
    Error(String),
}

/// Parse + validate one request line. Pure: no hardware, no I/O, fully
/// unit-testable. Bad JSON, an unknown op, or a missing required field resolve
/// to [`Parsed::Error`] with a stable `E_*` code; a `beep` pattern is clamped
/// into the safe bounds here so an out-of-range request is corrected, never
/// rejected outright (the operator still gets a bounded beep). An empty line is
/// a clean `E_BAD_REQUEST`, never a panic.
pub fn parse_command(line: &[u8]) -> Parsed {
    let req: Request = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(e) => return Parsed::Error(format!("E_BAD_REQUEST: {e}")),
    };
    match req.op.as_str() {
        "set" => {
            let pin = match req.pin {
                Some(p) => p,
                None => return Parsed::Error("E_MISSING_PIN".to_string()),
            };
            let level = match req.level {
                Some(l) => l,
                None => return Parsed::Error("E_MISSING_LEVEL".to_string()),
            };
            Parsed::Cmd(Command::Set {
                chip: req.chip.unwrap_or(0),
                pin,
                level,
            })
        }
        "beep" => {
            let pin = match req.pin {
                Some(p) => p,
                None => return Parsed::Error("E_MISSING_PIN".to_string()),
            };
            // A beep with no on/off envelope is meaningless; require at least
            // on_ms and a cycle count so a "beep" actually beeps.
            let on_ms = match req.on_ms {
                Some(v) => v,
                None => return Parsed::Error("E_MISSING_ON_MS".to_string()),
            };
            let cycles = match req.cycles {
                Some(v) => v,
                None => return Parsed::Error("E_MISSING_CYCLES".to_string()),
            };
            let pattern = BeepPattern {
                freq_hz: req.freq_hz.unwrap_or(0),
                duty_pct: req.duty_pct.unwrap_or(50),
                on_ms,
                off_ms: req.off_ms.unwrap_or(0),
                cycles,
            }
            .clamp();
            Parsed::Cmd(Command::Beep {
                chip: req.chip.unwrap_or(0),
                pin,
                pattern,
            })
        }
        "status" => Parsed::Cmd(Command::Status),
        other => Parsed::Error(format!("E_UNKNOWN_OP: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- BeepPattern::clamp -------------------------------------------------

    #[test]
    fn clamp_bounds_every_field() {
        let p = BeepPattern {
            freq_hz: 2_000,
            duty_pct: 200,
            on_ms: 99_999,
            off_ms: 99_999,
            cycles: 10_000,
        }
        .clamp();
        assert_eq!(p.duty_pct, 100);
        assert_eq!(p.on_ms, MAX_PHASE_MS);
        assert_eq!(p.off_ms, MAX_PHASE_MS);
        assert_eq!(p.cycles, MAX_BEEP_CYCLES);
        // freq_hz is advisory and not bounded by clamp.
        assert_eq!(p.freq_hz, 2_000);
    }

    #[test]
    fn clamp_is_idempotent() {
        let p = BeepPattern {
            freq_hz: 0,
            duty_pct: 50,
            on_ms: 100,
            off_ms: 100,
            cycles: 3,
        };
        assert_eq!(p.clamp(), p.clamp().clamp());
    }

    // ---- beep_schedule ------------------------------------------------------

    #[test]
    fn schedule_expands_cycles_into_high_low_phases() {
        let p = BeepPattern {
            freq_hz: 0,
            duty_pct: 50,
            on_ms: 100,
            off_ms: 50,
            cycles: 2,
        };
        let s = beep_schedule(p);
        // 2 cycles → High,Low,High,Low + the terminal Low.
        assert_eq!(
            s,
            vec![
                BeepPhase {
                    level: Level::High,
                    hold_ms: 100
                },
                BeepPhase {
                    level: Level::Low,
                    hold_ms: 50
                },
                BeepPhase {
                    level: Level::High,
                    hold_ms: 100
                },
                BeepPhase {
                    level: Level::Low,
                    hold_ms: 50
                },
                BeepPhase {
                    level: Level::Low,
                    hold_ms: 0
                },
            ]
        );
    }

    #[test]
    fn schedule_always_ends_low() {
        // Even a pure-on pattern (no off phase) ends with a terminal Low so the
        // line is never left high.
        let p = BeepPattern {
            freq_hz: 0,
            duty_pct: 100,
            on_ms: 200,
            off_ms: 0,
            cycles: 1,
        };
        let s = beep_schedule(p);
        assert_eq!(s.last().unwrap().level, Level::Low);
        // The High phase is present, the zero-ms Off is skipped, the terminal
        // Low is appended.
        assert_eq!(
            s[0],
            BeepPhase {
                level: Level::High,
                hold_ms: 200
            }
        );
        assert_eq!(s.last().unwrap().hold_ms, 0);
    }

    #[test]
    fn schedule_truncates_at_the_total_budget() {
        // A clamped pattern can still ask for 64 * (5000+5000) ms = 640 s; the
        // schedule must truncate so its summed duration never exceeds the 30 s
        // ceiling.
        let p = BeepPattern {
            freq_hz: 0,
            duty_pct: 100,
            on_ms: MAX_PHASE_MS,
            off_ms: MAX_PHASE_MS,
            cycles: MAX_BEEP_CYCLES,
        };
        let s = beep_schedule(p);
        let total: u64 = s.iter().map(|ph| ph.hold_ms as u64).sum();
        assert!(
            total <= MAX_TOTAL_MS,
            "scheduled {total} ms exceeds the cap"
        );
        assert_eq!(s.last().unwrap().level, Level::Low);
    }

    #[test]
    fn zero_cycle_pattern_is_a_single_terminal_low() {
        let p = BeepPattern {
            freq_hz: 0,
            duty_pct: 50,
            on_ms: 100,
            off_ms: 100,
            cycles: 0,
        };
        let s = beep_schedule(p);
        assert_eq!(
            s,
            vec![BeepPhase {
                level: Level::Low,
                hold_ms: 0
            }]
        );
    }

    // ---- parse_command ------------------------------------------------------

    fn cmd(line: &[u8]) -> Command {
        match parse_command(line) {
            Parsed::Cmd(c) => c,
            Parsed::Error(e) => panic!("expected a command, got error {e}"),
        }
    }

    fn err(line: &[u8]) -> String {
        match parse_command(line) {
            Parsed::Error(e) => e,
            Parsed::Cmd(c) => panic!("expected an error, got command {c:?}"),
        }
    }

    #[test]
    fn set_high_parses_with_a_default_chip() {
        assert_eq!(
            cmd(br#"{"op":"set","pin":17,"level":"high"}"#),
            Command::Set {
                chip: 0,
                pin: 17,
                level: Level::High
            }
        );
    }

    #[test]
    fn set_low_with_explicit_chip() {
        assert_eq!(
            cmd(br#"{"op":"set","chip":1,"pin":4,"level":"low"}"#),
            Command::Set {
                chip: 1,
                pin: 4,
                level: Level::Low
            }
        );
    }

    #[test]
    fn set_requires_pin_and_level() {
        assert_eq!(err(br#"{"op":"set","level":"high"}"#), "E_MISSING_PIN");
        assert_eq!(err(br#"{"op":"set","pin":17}"#), "E_MISSING_LEVEL");
    }

    #[test]
    fn beep_parses_and_clamps_the_pattern() {
        // An over-range request is clamped, not rejected: the operator still gets
        // a bounded beep.
        let c = cmd(
            br#"{"op":"beep","pin":18,"on_ms":99999,"off_ms":99999,"cycles":10000,"duty_pct":200}"#,
        );
        match c {
            Command::Beep { chip, pin, pattern } => {
                assert_eq!(chip, 0);
                assert_eq!(pin, 18);
                assert_eq!(pattern.on_ms, MAX_PHASE_MS);
                assert_eq!(pattern.off_ms, MAX_PHASE_MS);
                assert_eq!(pattern.cycles, MAX_BEEP_CYCLES);
                assert_eq!(pattern.duty_pct, 100);
            }
            other => panic!("expected a beep, got {other:?}"),
        }
    }

    #[test]
    fn beep_requires_an_envelope() {
        assert_eq!(err(br#"{"op":"beep","pin":18}"#), "E_MISSING_ON_MS");
        assert_eq!(
            err(br#"{"op":"beep","pin":18,"on_ms":100}"#),
            "E_MISSING_CYCLES"
        );
        assert_eq!(
            err(br#"{"op":"beep","on_ms":100,"cycles":3}"#),
            "E_MISSING_PIN"
        );
    }

    #[test]
    fn status_parses() {
        assert_eq!(cmd(br#"{"op":"status"}"#), Command::Status);
    }

    #[test]
    fn bad_json_is_a_clean_error_not_a_panic() {
        assert!(err(b"not json").starts_with("E_BAD_REQUEST"));
        assert!(err(b"").starts_with("E_BAD_REQUEST"));
    }

    #[test]
    fn unknown_op_is_rejected() {
        assert!(err(br#"{"op":"frob"}"#).starts_with("E_UNKNOWN_OP"));
    }

    #[test]
    fn an_invalid_level_string_is_a_bad_request() {
        // `level` only deserializes "low"/"high"; anything else fails the whole
        // request parse (a clean error, never a panic).
        assert!(err(br#"{"op":"set","pin":17,"level":"medium"}"#).starts_with("E_BAD_REQUEST"));
    }

    #[test]
    fn level_value_maps_to_the_line_value() {
        assert_eq!(Level::Low.value(), 0);
        assert_eq!(Level::High.value(), 1);
    }
}
