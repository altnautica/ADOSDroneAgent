//! Bounded line reader for child output.

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

/// The longest line kept, in bytes; the rest of a longer line is discarded up
/// to its terminator.
pub const MAX_LINE_BYTES: usize = 4096;

/// A line reader for child output that can neither grow without bound nor
/// wait forever on a newline, and never stops on invalid UTF-8.
///
/// `BufReader::lines` splits on `\n` only, buffers the whole line first and
/// ends the stream on the first non-UTF-8 byte. ffmpeg's periodic stats report
/// ends every update with `\r` and never with `\n`, so a healthy encoder's
/// stderr is one endless "line". This splits on either terminator, caps each
/// line at [`MAX_LINE_BYTES`] and decodes lossily.
pub struct BoundedLines<R> {
    reader: BufReader<R>,
    line: Vec<u8>,
    eof: bool,
}

impl<R: AsyncRead + Unpin> BoundedLines<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
            line: Vec::new(),
            eof: false,
        }
    }

    /// The next `\n`- or `\r`-terminated line (possibly empty), truncated to
    /// [`MAX_LINE_BYTES`]. `None` once the stream has ended or failed and any
    /// unterminated tail has been returned.
    pub async fn next_line(&mut self) -> Option<String> {
        loop {
            if self.eof {
                if self.line.is_empty() {
                    return None;
                }
                return Some(self.take_line());
            }
            let buf = match self.reader.fill_buf().await {
                Ok(buf) if !buf.is_empty() => buf,
                _ => {
                    self.eof = true;
                    continue;
                }
            };
            match buf.iter().position(|b| *b == b'\n' || *b == b'\r') {
                Some(end) => {
                    push_bounded(&mut self.line, &buf[..end]);
                    self.reader.consume(end + 1);
                    return Some(self.take_line());
                }
                None => {
                    let n = buf.len();
                    push_bounded(&mut self.line, buf);
                    self.reader.consume(n);
                }
            }
        }
    }

    fn take_line(&mut self) -> String {
        let text = String::from_utf8_lossy(&self.line).into_owned();
        self.line.clear();
        text
    }
}

fn push_bounded(line: &mut Vec<u8>, bytes: &[u8]) {
    let room = MAX_LINE_BYTES.saturating_sub(line.len());
    line.extend_from_slice(&bytes[..bytes.len().min(room)]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn carriage_return_status_lines_are_split_and_every_line_is_bounded() {
        let mut stream = Vec::new();
        for i in 0..2000 {
            stream.extend_from_slice(format!("frame={i} fps=30 q=23.0 size=1kB   \r").as_bytes());
        }
        stream.extend(std::iter::repeat_n(b'x', 1 << 20));
        let mut lines = BoundedLines::new(&stream[..]);
        let mut count = 0usize;
        let mut longest = 0usize;
        while let Some(line) = lines.next_line().await {
            count += 1;
            longest = longest.max(line.len());
        }
        assert_eq!(count, 2001, "each \\r-terminated update is its own line");
        assert!(longest <= MAX_LINE_BYTES, "a line is capped, got {longest}");
    }

    #[tokio::test]
    async fn invalid_utf8_does_not_end_the_stream() {
        let stream = b"one\n\xff\xfe\ntwo\n";
        let mut lines = BoundedLines::new(&stream[..]);
        assert_eq!(lines.next_line().await.as_deref(), Some("one"));
        assert!(lines.next_line().await.is_some());
        assert_eq!(lines.next_line().await.as_deref(), Some("two"));
        assert_eq!(lines.next_line().await, None);
    }
}
