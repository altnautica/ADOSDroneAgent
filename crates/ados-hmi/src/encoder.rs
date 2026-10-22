//! Rotary encoder (KY-040 class): quadrature decode and the encoder's labels.
//!
//! A KY-040 is a mechanical incremental encoder with two switched outputs, `CLK`
//! (line A here) and `DT` (line B), pulled up on the module, plus a push switch
//! (`SW`) to ground. At every detent both outputs rest high. One detent of
//! rotation walks the 2-bit Gray code (`A << 1 | B`) through all four states
//! and back to rest: clockwise A leads (`11 -> 01 -> 00 -> 10 -> 11`),
//! counter-clockwise B leads (`11 -> 10 -> 00 -> 01 -> 11`).
//!
//! [`QuadratureDecoder`] counts each valid single-line transition as one quarter
//! step (+1 clockwise, -1 counter-clockwise) and reports a detent only when the
//! code returns to rest with a net count of [`STEPS_PER_DETENT`] in one
//! direction. Contact bounce flips one line back and forth, adding and removing
//! the same step, so it cancels out instead of producing a detent. A transition
//! where both lines changed at once (an edge was missed) has no direction: it is
//! rejected and the partial detent is discarded.
//!
//! The push switch is an ordinary active-low button: it runs through the shared
//! [`crate::press::PressClassifier`] (debounce, short/long/cancel) under [`ENC_SW`].
//! A detent is delivered as a `short` press of [`ENC_CW`] or [`ENC_CCW`], so
//! every consumer of the button stream reads it without a new event kind.

/// Label of one clockwise detent.
pub const ENC_CW: &str = "ENC_CW";

/// Label of one counter-clockwise detent.
pub const ENC_CCW: &str = "ENC_CCW";

/// Label of the encoder's push switch.
pub const ENC_SW: &str = "ENC_SW";

/// Quarter steps in one KY-040 detent (one full Gray-code cycle).
pub const STEPS_PER_DETENT: i8 = 4;

/// The code both outputs rest at between detents (both lines high).
const REST: u8 = 0b11;

/// One decoded detent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    /// Clockwise: the next screen by default.
    Cw,
    /// Counter-clockwise: the previous screen by default.
    Ccw,
}

impl Rotation {
    /// The front-panel label this detent is delivered under.
    pub fn label(self) -> &'static str {
        match self {
            Rotation::Cw => ENC_CW,
            Rotation::Ccw => ENC_CCW,
        }
    }
}

/// Which quadrature output an edge arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// The module's `CLK` output.
    A,
    /// The module's `DT` output.
    B,
}

/// The 2-bit code of the two line levels (`true` = high).
fn code_of(a: bool, b: bool) -> u8 {
    (u8::from(a) << 1) | u8::from(b)
}

/// The quarter step a move from code `from` to code `to` represents: `Some(0)`
/// for no change, `Some(1)` clockwise, `Some(-1)` counter-clockwise, `None` for a
/// move where both lines changed (no direction can be read from it).
fn quarter_step(from: u8, to: u8) -> Option<i8> {
    match (from, to) {
        _ if from == to => Some(0),
        (0b11, 0b01) | (0b01, 0b00) | (0b00, 0b10) | (0b10, 0b11) => Some(1),
        (0b11, 0b10) | (0b10, 0b00) | (0b00, 0b01) | (0b01, 0b11) => Some(-1),
        _ => None,
    }
}

/// Full-detent quadrature decoder over the two encoder lines.
#[derive(Debug, Clone)]
pub struct QuadratureDecoder {
    /// The last accepted code.
    code: u8,
    /// Net quarter steps since the code last sat at rest.
    steps: i8,
    /// Transitions rejected because both lines changed at once.
    rejected: u64,
}

impl QuadratureDecoder {
    /// A decoder starting from the lines' current levels (`true` = high).
    pub fn new(a: bool, b: bool) -> Self {
        Self {
            code: code_of(a, b),
            steps: 0,
            rejected: 0,
        }
    }

    /// Feed one edge: `channel` now reads `high`. Returns the detent it
    /// completes, if any.
    pub fn on_edge(&mut self, channel: Channel, high: bool) -> Option<Rotation> {
        let a = self.code & 0b10 != 0;
        let b = self.code & 0b01 != 0;
        match channel {
            Channel::A => self.on_levels(high, b),
            Channel::B => self.on_levels(a, high),
        }
    }

    /// Feed both line levels at once. Returns the detent the new state
    /// completes, if any.
    pub fn on_levels(&mut self, a: bool, b: bool) -> Option<Rotation> {
        let code = code_of(a, b);
        match quarter_step(self.code, code) {
            Some(0) => return None,
            Some(step) => self.steps = self.steps.saturating_add(step),
            None => {
                // Both lines moved: the direction is unknowable, so the partial
                // detent is dropped and counting restarts from the new code.
                self.rejected += 1;
                self.steps = 0;
                self.code = code;
                return None;
            }
        }
        self.code = code;
        if code != REST {
            return None;
        }
        match std::mem::take(&mut self.steps) {
            STEPS_PER_DETENT => Some(Rotation::Cw),
            s if s == -STEPS_PER_DETENT => Some(Rotation::Ccw),
            _ => None,
        }
    }

    /// How many transitions were rejected as invalid (both lines changed).
    pub fn rejected(&self) -> u64 {
        self.rejected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a sequence of `(a, b)` levels, collecting the detents.
    fn feed(d: &mut QuadratureDecoder, levels: &[(u8, u8)]) -> Vec<Rotation> {
        levels
            .iter()
            .filter_map(|&(a, b)| d.on_levels(a == 1, b == 1))
            .collect()
    }

    const CW: [(u8, u8); 4] = [(0, 1), (0, 0), (1, 0), (1, 1)];
    const CCW: [(u8, u8); 4] = [(1, 0), (0, 0), (0, 1), (1, 1)];

    #[test]
    fn a_full_clockwise_cycle_is_one_cw_detent() {
        let mut d = QuadratureDecoder::new(true, true);
        assert_eq!(feed(&mut d, &CW), vec![Rotation::Cw]);
        assert_eq!(feed(&mut d, &CW), vec![Rotation::Cw]);
        assert_eq!(d.rejected(), 0);
    }

    #[test]
    fn a_full_counter_clockwise_cycle_is_one_ccw_detent() {
        let mut d = QuadratureDecoder::new(true, true);
        assert_eq!(feed(&mut d, &CCW), vec![Rotation::Ccw]);
        assert_eq!(feed(&mut d, &CCW), vec![Rotation::Ccw]);
    }

    #[test]
    fn a_detent_fires_only_on_the_return_to_rest() {
        let mut d = QuadratureDecoder::new(true, true);
        // Three quarter steps are not a detent yet.
        assert!(feed(&mut d, &CW[..3]).is_empty());
        assert_eq!(d.on_levels(true, true), Some(Rotation::Cw));
    }

    #[test]
    fn edges_on_each_channel_decode_the_same_as_levels() {
        let mut d = QuadratureDecoder::new(true, true);
        // Clockwise: A falls, B falls, A rises, B rises.
        assert_eq!(d.on_edge(Channel::A, false), None);
        assert_eq!(d.on_edge(Channel::B, false), None);
        assert_eq!(d.on_edge(Channel::A, true), None);
        assert_eq!(d.on_edge(Channel::B, true), Some(Rotation::Cw));
        // Counter-clockwise: B leads.
        assert_eq!(d.on_edge(Channel::B, false), None);
        assert_eq!(d.on_edge(Channel::A, false), None);
        assert_eq!(d.on_edge(Channel::B, true), None);
        assert_eq!(d.on_edge(Channel::A, true), Some(Rotation::Ccw));
    }

    #[test]
    fn a_repeated_level_is_not_a_step() {
        let mut d = QuadratureDecoder::new(true, true);
        // A duplicate edge report (same level twice) changes nothing.
        assert_eq!(d.on_edge(Channel::A, false), None);
        assert_eq!(d.on_edge(Channel::A, false), None);
        assert_eq!(feed(&mut d, &CW[1..]), vec![Rotation::Cw]);
    }

    #[test]
    fn contact_bounce_cancels_out_instead_of_counting() {
        let mut d = QuadratureDecoder::new(true, true);
        // A chatters at the start of the detent: 11 -> 01 -> 11 -> 01 ...
        let chatter = [(0, 1), (1, 1), (0, 1), (1, 1), (0, 1)];
        assert!(feed(&mut d, &chatter).is_empty());
        // ... then the detent completes once, with bounce in the middle too.
        let rest = [(0, 0), (0, 1), (0, 0), (1, 0), (1, 1)];
        assert_eq!(feed(&mut d, &rest), vec![Rotation::Cw]);
        assert_eq!(d.rejected(), 0);
    }

    #[test]
    fn a_half_turn_and_back_is_no_detent() {
        let mut d = QuadratureDecoder::new(true, true);
        // Two steps clockwise, then back the way it came.
        let wobble = [(0, 1), (0, 0), (0, 1), (1, 1)];
        assert!(feed(&mut d, &wobble).is_empty());
        // The count was cleared at rest: the next clean detent is exact.
        assert_eq!(feed(&mut d, &CCW), vec![Rotation::Ccw]);
    }

    #[test]
    fn a_both_lines_transition_is_rejected_and_drops_the_partial_detent() {
        let mut d = QuadratureDecoder::new(true, true);
        // 11 -> 01 (valid), then 01 -> 10 (both lines moved: invalid).
        assert!(feed(&mut d, &[(0, 1), (1, 0)]).is_empty());
        assert_eq!(d.rejected(), 1);
        // 10 -> 11 is a valid quarter step but the detent was dropped.
        assert_eq!(d.on_levels(true, true), None);
        // 11 -> 00 straight across is invalid too.
        assert_eq!(d.on_levels(false, false), None);
        assert_eq!(d.rejected(), 2);
        // Back at rest, a clean detent decodes normally again.
        assert_eq!(d.on_levels(true, true), None);
        assert_eq!(d.rejected(), 3);
        assert_eq!(feed(&mut d, &CW), vec![Rotation::Cw]);
    }

    #[test]
    fn a_decoder_started_mid_detent_never_reports_a_false_detent() {
        // The reader started with the knob between detents (code 00).
        let mut d = QuadratureDecoder::new(false, false);
        // Finishing that partial clockwise detent is two steps, not four.
        assert!(feed(&mut d, &[(1, 0), (1, 1)]).is_empty());
        assert_eq!(feed(&mut d, &CW), vec![Rotation::Cw]);
    }

    #[test]
    fn detents_carry_the_encoder_labels() {
        assert_eq!(Rotation::Cw.label(), "ENC_CW");
        assert_eq!(Rotation::Ccw.label(), "ENC_CCW");
        assert_eq!(ENC_SW, "ENC_SW");
    }
}
