//! Copy to the operator's clipboard with the OSC 52 terminal escape.
//!
//! OSC 52 asks the terminal emulator itself to set the clipboard, so a copy
//! works over SSH and inside tmux (with `set-clipboard on`) without any
//! clipboard tool on the agent host.

use std::io::Write;

use base64::Engine;

/// The OSC 52 sequence that sets the clipboard to `text`.
pub fn osc52(text: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{encoded}\x07")
}

/// Write `text` to the clipboard through the terminal.
pub fn copy(text: &str) {
    let mut out = std::io::stdout();
    let _ = out.write_all(osc52(text).as_bytes());
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_wraps_base64() {
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }
}
