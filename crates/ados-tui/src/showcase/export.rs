//! Buffer → styled spans: the replay format the website draws.

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use serde::Serialize;

/// Terminal size every frame is drawn at.
pub(super) const WIDTH: u16 = 120;
pub(super) const HEIGHT: u16 = 40;
/// How long the replay holds each frame.
const HOLD_MS: u32 = 2400;

/// A run of cells sharing one style.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(super) struct Span {
    pub text: String,
    pub fg: Option<String>,
    pub bg: Option<String>,
    pub bold: bool,
    pub dim: bool,
}

#[derive(Serialize)]
struct Crop {
    x: u16,
    y: u16,
    w: u16,
    h: u16,
}

#[derive(Serialize)]
struct Frame {
    name: &'static str,
    hold_ms: u32,
    lines: Vec<Vec<Span>>,
}

#[derive(Serialize)]
struct Document {
    width: u16,
    height: u16,
    mobile_crop: Crop,
    frames: Vec<Frame>,
}

/// `#RRGGBB` for a truecolor cell, `None` for the terminal default. Any other
/// colour means a widget bypassed the theme: the export refuses it.
fn hex(c: Color) -> Option<String> {
    match c {
        Color::Rgb(r, g, b) => Some(format!("#{r:02X}{g:02X}{b:02X}")),
        Color::Reset => None,
        other => panic!("non-truecolor colour in an exported frame: {other:?}"),
    }
}

/// Every row of `buf` as merged style runs. A double-width glyph consumes the
/// cell after it; empty cells (wide-glyph continuations) are skipped.
pub(super) fn to_frames(buf: &Buffer) -> Vec<Vec<Span>> {
    let area = buf.area;
    (area.top()..area.bottom())
        .map(|y| {
            let mut row: Vec<Span> = Vec::new();
            let mut x = area.left();
            while x < area.right() {
                let cell = &buf[(x, y)];
                let sym = cell.symbol();
                x += 1;
                if sym.is_empty() {
                    continue;
                }
                if ratatui::text::Span::raw(sym).width() == 2 {
                    x += 1;
                }
                let (mut fg, mut bg) = (hex(cell.fg), hex(cell.bg));
                if cell.modifier.contains(Modifier::REVERSED) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                let bold = cell.modifier.contains(Modifier::BOLD);
                let dim = cell.modifier.contains(Modifier::DIM);
                match row.last_mut() {
                    Some(s) if s.fg == fg && s.bg == bg && s.bold == bold && s.dim == dim => {
                        s.text.push_str(sym)
                    }
                    _ => row.push(Span {
                        text: sym.to_string(),
                        fg,
                        bg,
                        bold,
                        dim,
                    }),
                }
            }
            row
        })
        .collect()
}

/// The plain text of exported lines, one row per line.
pub(super) fn text(lines: &[Vec<Span>]) -> String {
    let mut out = String::new();
    for row in lines {
        for s in row {
            out.push_str(&s.text);
        }
        out.push('\n');
    }
    out
}

/// The replay document, pretty-printed.
pub(super) fn document(frames: Vec<(&'static str, Vec<Vec<Span>>)>) -> String {
    let doc = Document {
        width: WIDTH,
        height: HEIGHT,
        mobile_crop: Crop {
            x: 0,
            y: 0,
            w: 60,
            h: 16,
        },
        frames: frames
            .into_iter()
            .map(|(name, lines)| Frame {
                name,
                hold_ms: HOLD_MS,
                lines,
            })
            .collect(),
    };
    serde_json::to_string_pretty(&doc).expect("the replay document serialises")
}
