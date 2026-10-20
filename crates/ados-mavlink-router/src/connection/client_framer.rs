//! Reassembly of a client's byte stream into whole MAVLink frames.
//!
//! A stream client (a TCP or WebSocket GCS, a writer on the on-box IPC
//! socket) may split one frame across two writes. [`ClientFramer`] buffers
//! the partial frame until the rest arrives, so a frame is never split across
//! two writes to the flight controller and the visor flight-action filter
//! always sees whole frames. [`ConnFramers`] keeps one framer per connection
//! for a lane whose writers share one channel. A UDP datagram carries whole
//! frames by itself and is not reassembled.

use std::collections::HashMap;

use super::{extract_frames, FC_MAX_SINGLE_WRITE};

/// Upper bound on a client's reassembly buffer. A client frame is at most 280
/// bytes, so this only trips on a stream that is not MAVLink at all.
const CLIENT_BUF_CAP: usize = 64 * 1024;

/// Connections that may hold a partial frame at once on a shared lane. A
/// connection that closed mid-frame leaves its entry behind; past this many
/// the stale entries are dropped.
const MAX_PENDING_CONNS: usize = 64;

/// Reassembles one client's byte stream into whole MAVLink frames.
#[derive(Debug, Default)]
pub(crate) struct ClientFramer {
    buf: Vec<u8>,
}

impl ClientFramer {
    /// Feed `data`; returns the complete frames it finished, concatenated into
    /// batches of at most [`FC_MAX_SINGLE_WRITE`] bytes. A partial trailing
    /// frame stays buffered for the next call.
    pub(crate) fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(data);
        let frames = extract_frames(&mut self.buf);
        if self.buf.len() > CLIENT_BUF_CAP {
            self.buf.clear();
        }
        let mut batches = Vec::new();
        let mut batch: Vec<u8> = Vec::new();
        for frame in frames {
            if !batch.is_empty() && batch.len() + frame.len() > FC_MAX_SINGLE_WRITE {
                batches.push(std::mem::take(&mut batch));
            }
            batch.extend_from_slice(&frame);
        }
        if !batch.is_empty() {
            batches.push(batch);
        }
        batches
    }

    /// Whether no partial frame is buffered.
    fn is_idle(&self) -> bool {
        self.buf.is_empty()
    }
}

/// One [`ClientFramer`] per connection of a shared lane, keyed by the
/// connection serial. An entry lives only while its connection holds a
/// partial frame.
#[derive(Debug, Default)]
pub(crate) struct ConnFramers {
    by_conn: HashMap<u64, ClientFramer>,
}

impl ConnFramers {
    /// Feed `data` written by connection `conn`; returns its finished frames
    /// batched as [`ClientFramer::push`] does.
    pub(crate) fn push(&mut self, conn: u64, data: &[u8]) -> Vec<Vec<u8>> {
        let framer = self.by_conn.entry(conn).or_default();
        let batches = framer.push(data);
        if framer.is_idle() {
            self.by_conn.remove(&conn);
        } else if self.by_conn.len() > MAX_PENDING_CONNS {
            self.by_conn.retain(|c, _| *c == conn);
        }
        batches
    }

    /// Connections currently holding a partial frame.
    #[cfg(test)]
    fn pending(&self) -> usize {
        self.by_conn.len()
    }
}

#[cfg(test)]
mod tests;
