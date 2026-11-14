//! How a state word reads on the Vehicle tab.

/// How a state word reads: good, a caution, a fault, or neutral. The panel
/// pairs each with a symbol and a colour, and always prints the word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good,
    Caution,
    Fault,
    Neutral,
}

/// The tone of an external-nav health word.
pub fn health_tone(health: &str) -> Tone {
    match health {
        "good" => Tone::Good,
        "degraded" => Tone::Caution,
        "lost" => Tone::Fault,
        _ => Tone::Neutral,
    }
}
