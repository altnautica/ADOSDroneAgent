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

use super::{
    ButtonEvent, ButtonLine, Channel, Edge, EncoderLines, PressClassifier, QuadratureDecoder,
};

/// What a requested line feeds.
#[derive(Debug, Clone, Copy)]
enum Input {
    /// A button or the encoder push switch, by GPIO number: the press
    /// classifier.
    Press(u32),
    /// One encoder quadrature output: the decoder.
    Quadrature(Channel),
}

/// One edge as a reader thread reports it.
type EdgeMsg = anyhow::Result<(Input, Edge, u64)>;

/// Run the GPIO edge-event read loop, classifying button and encoder-switch
/// presses and decoding encoder detents, and invoking `on_event` with the
/// front-panel label and the event for each one. Opens each line's own
/// `/dev/gpiochip<N>` and requests both edges. Returns when every line's reader
/// has ended, or with the first read error. The chip-open is the only
/// hardware-coupled step; classification and decoding stay in
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
    let mut decoder = None;
    if let Some(enc) = encoder {
        let a = spawn_reader(&mut chips, &enc.a, Input::Quadrature(Channel::A), base, &tx)?;
        let b = spawn_reader(&mut chips, &enc.b, Input::Quadrature(Channel::B), base, &tx)?;
        spawn_reader(&mut chips, &enc.sw, Input::Press(enc.sw.gpio), base, &tx)?;
        decoder = Some(QuadratureDecoder::new(a != 0, b != 0));
    }
    // Only the reader threads hold senders now, so the loop ends when they do.
    drop(tx);

    for msg in rx {
        let (input, edge, ts_ms) = msg?;
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
                    (input, edge, base.elapsed().as_millis() as u64)
                });
                let failed = msg.is_err();
                if tx.send(msg).is_err() || failed {
                    return;
                }
            }
        })
        .context("spawn gpio reader thread")?;
    Ok(level)
}
