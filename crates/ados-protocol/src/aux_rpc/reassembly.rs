//! Reassembly of fragmented requests.

use super::*;

/// What one pushed fragment settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReassemblyOutcome {
    /// Every fragment is in and the object decoded.
    Complete(RpcRequest),
    /// More fragments are needed.
    Pending,
    /// Already held, or a fragment of a request this reassembler dropped.
    Duplicate,
    /// Dropped: the request would exceed a buffering bound, or its object did
    /// not decode.
    Rejected,
}

pub(super) struct PartialRequest {
    method: RpcMethod,
    target: Vec<u8>,
    total: u16,
    chunks: Vec<Option<Vec<u8>>>,
    received: u16,
    bytes: usize,
    started: std::time::Instant,
}

/// Collects request fragments on the drone until a request is whole.
///
/// Bounded in count, bytes and time: the uplink is a broadcast any fleet member
/// can transmit on, and nothing in a fragment is authenticated until the whole
/// object (and the ticket inside it) has arrived.
#[derive(Default)]
pub struct RequestReassembler {
    partials: std::collections::HashMap<u32, PartialRequest>,
    buffered: usize,
}

impl RequestReassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one fragment received at `now`.
    pub fn push(
        &mut self,
        fragment: &RequestFragment<'_>,
        now: std::time::Instant,
    ) -> ReassemblyOutcome {
        self.expire(now);
        if fragment.total == 1 {
            return match decode_request_object(
                fragment.id,
                fragment.method,
                fragment.target,
                fragment.chunk,
            ) {
                Ok(request) => ReassemblyOutcome::Complete(request),
                Err(_) => ReassemblyOutcome::Rejected,
            };
        }
        if fragment.chunk.len() > MAX_REQUEST_CHUNK {
            return ReassemblyOutcome::Rejected;
        }
        // A fragment that disagrees with the partial already held under its id
        // is a different request reusing the id (a restarted ground station);
        // the newer one wins.
        let conflicting = self.partials.get(&fragment.id).is_some_and(|p| {
            p.method != fragment.method || p.total != fragment.total || p.target != fragment.target
        });
        if conflicting {
            self.drop_partial(fragment.id);
        }
        if !self.partials.contains_key(&fragment.id) {
            if self.partials.len() >= MAX_PARTIAL_REQUESTS {
                return ReassemblyOutcome::Rejected;
            }
            self.partials.insert(
                fragment.id,
                PartialRequest {
                    method: fragment.method,
                    target: fragment.target.to_vec(),
                    total: fragment.total,
                    chunks: vec![None; fragment.total as usize],
                    received: 0,
                    bytes: 0,
                    started: now,
                },
            );
        }
        let buffered = self.buffered;
        let Some(partial) = self.partials.get_mut(&fragment.id) else {
            return ReassemblyOutcome::Rejected;
        };
        if partial.chunks[fragment.seq as usize].is_some() {
            return ReassemblyOutcome::Duplicate;
        }
        if buffered + fragment.chunk.len() > MAX_PARTIAL_REQUEST_BYTES
            || partial.bytes + fragment.chunk.len() > MAX_REQUEST_OBJECT
        {
            self.drop_partial(fragment.id);
            return ReassemblyOutcome::Rejected;
        }
        partial.chunks[fragment.seq as usize] = Some(fragment.chunk.to_vec());
        partial.received += 1;
        partial.bytes += fragment.chunk.len();
        self.buffered += fragment.chunk.len();
        if partial.received < partial.total {
            return ReassemblyOutcome::Pending;
        }
        let Some(done) = self.partials.remove(&fragment.id) else {
            return ReassemblyOutcome::Rejected;
        };
        self.buffered = self.buffered.saturating_sub(done.bytes);
        let mut object = Vec::with_capacity(done.bytes);
        for chunk in done.chunks.into_iter().flatten() {
            object.extend_from_slice(&chunk);
        }
        match decode_request_object(fragment.id, done.method, &done.target, &object) {
            Ok(request) => ReassemblyOutcome::Complete(request),
            Err(_) => ReassemblyOutcome::Rejected,
        }
    }

    /// Requests currently partially held.
    pub fn partial_count(&self) -> usize {
        self.partials.len()
    }

    pub(super) fn drop_partial(&mut self, id: u32) {
        if let Some(p) = self.partials.remove(&id) {
            self.buffered = self.buffered.saturating_sub(p.bytes);
        }
    }

    pub(super) fn expire(&mut self, now: std::time::Instant) {
        let stale: Vec<u32> = self
            .partials
            .iter()
            .filter(|(_, p)| now.saturating_duration_since(p.started) > REQUEST_REASSEMBLY_TIMEOUT)
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.drop_partial(id);
        }
    }
}
