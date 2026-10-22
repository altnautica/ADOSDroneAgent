//! The Linux character-device read loop for the panel buttons and encoder.
//!
//! Every requested line gets its own reader thread blocked on its edge events;
//! the threads funnel `(input, edge, ts_ms)` into one channel, so a single loop
//! owns the classifier and the quadrature decoder and an edge on any line is
//! handled as it arrives (a line with no activity never holds up the others).

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Instant;

use anyhow::Context;
use gpio_cdev::{Chip, EventRequestFlags, EventType, LineRequestFlags};

use ados_hmi::encoder::{Channel, QuadratureDecoder};
use ados_hmi::press::{ButtonEvent, Edge, PressClassifier};

use super::{ButtonLine, EncoderLines};

/// What a requested line feeds.
#[derive(Debug, Clone, Copy)]
enum Input {
    /// A button or the encoder push switch, by GPIO number: the press
    /// classifier.
    Press(u32),
    /// One encoder quadrature output: the decoder.
    Quadrature(Channel),
}

/// One edge, or the failure that ended a line's reader, tagged with the line.
type EdgeMsg = (Input, anyhow::Result<(Edge, u64)>);

/// What a line failure means for the read loop.
#[derive(Debug, PartialEq, Eq)]
enum OnFailure {
    /// A button line: the loop ends with the error (the unit restarts it).
    Stop,
    /// An encoder line: the encoder is dropped, the buttons keep working.
    DropEncoder,
}

/// The loop's response to a failure of the line feeding `input`, on a board
/// whose encoder switch is `encoder_sw`.
fn failure_action(input: Input, encoder_sw: Option<u32>) -> OnFailure {
    match input {
        Input::Quadrature(_) => OnFailure::DropEncoder,
        Input::Press(pin) if Some(pin) == encoder_sw => OnFailure::DropEncoder,
        Input::Press(_) => OnFailure::Stop,
    }
}

/// Request the encoder's three lines through `spawn`. A line that cannot be
/// requested (busy, missing) leaves the encoder, or just its switch,
/// unavailable and is logged; it never stops the buttons. Returns the decoder
/// when both quadrature lines are being read.
fn start_encoder(
    enc: &EncoderLines,
    mut spawn: impl FnMut(&ButtonLine, Input) -> anyhow::Result<u8>,
) -> Option<QuadratureDecoder> {
    let a = spawn(&enc.a, Input::Quadrature(Channel::A));
    let b = spawn(&enc.b, Input::Quadrature(Channel::B));
    if let Err(e) = spawn(&enc.sw, Input::Press(enc.sw.gpio)) {
        tracing::warn!(error = %format!("{e:#}"), "encoder_switch_unavailable");
    }
    match (a, b) {
        (Ok(a), Ok(b)) => Some(QuadratureDecoder::new(a != 0, b != 0)),
        (a, b) => {
            for e in [a.err(), b.err()].into_iter().flatten() {
                tracing::warn!(error = %format!("{e:#}"), "encoder_unavailable");
            }
            None
        }
    }
}

/// Run the GPIO edge-event read loop, classifying button and encoder-switch
/// presses and decoding encoder detents, and invoking `on_event` with the
/// front-panel label and the event for each one. Opens each line's own
/// `/dev/gpiochip<N>` and requests both edges. Returns when every line's reader
/// has ended, or with the first button-line error; an encoder line that
/// cannot be requested or read only drops the encoder. The chip-open is the
/// only hardware-coupled step; classification and decoding stay in
/// [`PressClassifier`] and [`QuadratureDecoder`].
pub fn run_event_loop<F>(
    buttons: &[ButtonLine],
    encoder: Option<&EncoderLines>,
    classifier: &mut PressClassifier,
    mut on_event: F,
) -> anyhow::Result<()>
where
    F: FnMut(String, ButtonEvent),
{
    let base = Instant::now();
    let mut chips: HashMap<u32, Chip> = HashMap::new();
    let (tx, rx) = mpsc::channel::<EdgeMsg>();

    for button in buttons {
        spawn_reader(&mut chips, button, Input::Press(button.gpio), base, &tx)?;
    }
    let mut decoder = encoder.and_then(|enc| {
        start_encoder(enc, |line, input| {
            spawn_reader(&mut chips, line, input, base, &tx)
        })
    });
    let encoder_sw = encoder.map(|enc| enc.sw.gpio);
    // Only the reader threads hold senders now, so the loop ends when they do.
    drop(tx);

    for (input, msg) in rx {
        let (edge, ts_ms) = match msg {
            Ok(edge) => edge,
            Err(e) => match failure_action(input, encoder_sw) {
                OnFailure::Stop => return Err(e),
                OnFailure::DropEncoder => {
                    tracing::warn!(error = %format!("{e:#}"), "encoder_unavailable");
                    if matches!(input, Input::Quadrature(_)) {
                        decoder = None;
                    }
                    continue;
                }
            },
        };
        match input {
            Input::Press(pin) => {
                if let Some(ev) = classifier.on_edge(pin, edge, ts_ms) {
                    on_event(classifier.label(pin), ev);
                }
            }
            Input::Quadrature(channel) => {
                let (Some(decoder), Some(enc)) = (decoder.as_mut(), encoder) else {
                    continue;
                };
                if let Some(rotation) = decoder.on_edge(channel, edge == Edge::Rising) {
                    let ev = classifier.on_rotation(rotation, enc.a.gpio, ts_ms);
                    on_event(rotation.label().to_string(), ev);
                }
            }
        }
    }
    Ok(())
}

/// Request both edges on `line`, start a thread forwarding its edges into
/// `tx`, and return the line's level at request time.
fn spawn_reader(
    chips: &mut HashMap<u32, Chip>,
    line: &ButtonLine,
    input: Input,
    base: Instant,
    tx: &mpsc::Sender<EdgeMsg>,
) -> anyhow::Result<u8> {
    let chip = match chips.entry(line.chip) {
        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
        std::collections::hash_map::Entry::Vacant(e) => e.insert(
            Chip::new(line.chip_path()).with_context(|| format!("open {}", line.chip_path()))?,
        ),
    };
    let events = chip
        .get_line(line.line)
        .and_then(|gpio_line| {
            gpio_line.events(
                LineRequestFlags::INPUT,
                EventRequestFlags::BOTH_EDGES,
                "ados-pic-buttons",
            )
        })
        .with_context(|| format!("request {} line {}", line.chip_path(), line.line))?;
    let level = events
        .get_value()
        .with_context(|| format!("read {} line {}", line.chip_path(), line.line))?;
    let tx = tx.clone();
    std::thread::Builder::new()
        .name(format!("ados-gpio-{}", line.gpio))
        .spawn(move || {
            for evt in events {
                let msg = evt.map_err(anyhow::Error::from).map(|evt| {
                    let edge = match evt.event_type() {
                        EventType::FallingEdge => Edge::Falling,
                        EventType::RisingEdge => Edge::Rising,
                    };
                    (edge, base.elapsed().as_millis() as u64)
                });
                let failed = msg.is_err();
                if tx.send((input, msg)).is_err() || failed {
                    return;
                }
            }
        })
        .context("spawn gpio reader thread")?;
    Ok(level)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(gpio: u32) -> ButtonLine {
        ButtonLine {
            gpio,
            chip: 4,
            line: gpio - 128,
        }
    }

    fn encoder() -> EncoderLines {
        EncoderLines {
            a: line(130),
            b: line(133),
            sw: line(140),
        }
    }

    /// Start the encoder with every line in `busy` failing to request.
    fn start_with_busy(busy: &[u32]) -> (Option<QuadratureDecoder>, Vec<u32>) {
        let mut requested = Vec::new();
        let decoder = start_encoder(&encoder(), |l, _| {
            requested.push(l.gpio);
            if busy.contains(&l.gpio) {
                anyhow::bail!("request line {}: EBUSY", l.line)
            }
            Ok(1)
        });
        (decoder, requested)
    }

    #[test]
    fn a_busy_encoder_line_leaves_the_encoder_unavailable_not_an_error() {
        let (decoder, requested) = start_with_busy(&[]);
        assert!(decoder.is_some());
        assert_eq!(requested, [130, 133, 140]);
        // A quadrature line busy: no decoder, the switch is still requested.
        for busy in [130, 133] {
            let (decoder, requested) = start_with_busy(&[busy]);
            assert!(decoder.is_none(), "{busy}");
            assert_eq!(requested, [130, 133, 140], "{busy}");
        }
        // Only the switch busy: rotation still works.
        assert!(start_with_busy(&[140]).0.is_some());
    }

    #[test]
    fn only_a_button_line_failure_stops_the_loop() {
        let sw = Some(140);
        assert_eq!(failure_action(Input::Press(17), sw), OnFailure::Stop);
        assert_eq!(failure_action(Input::Press(17), None), OnFailure::Stop);
        assert_eq!(
            failure_action(Input::Press(140), sw),
            OnFailure::DropEncoder
        );
        for channel in [Channel::A, Channel::B] {
            assert_eq!(
                failure_action(Input::Quadrature(channel), sw),
                OnFailure::DropEncoder
            );
        }
    }
}
