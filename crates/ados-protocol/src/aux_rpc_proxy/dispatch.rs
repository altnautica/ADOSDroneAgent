//! Routing inbound responses to pending callers.

use super::*;

impl AuxRpcProxy {
    /// Route a decoded response fragment to its pending caller, completing the
    /// call as soon as RaptorQ can rebuild the body from what has arrived.
    ///
    /// Called by the response reader task when it decodes a Response payload
    /// off the IPC socket. A fragment whose id matches no pending entry is a
    /// no-op: the caller already gave up (timeout) or the proxy was reset,
    /// both expected on a reordering lane.
    ///
    /// Completion is on sufficiency, not on `total`: a response carries `k`
    /// systematic plus [`aux_rpc::RPC_REPAIR_SYMBOLS`] repair symbols, and
    /// waiting for every index is exactly the geometric loss decay the repair
    /// symbols exist to remove.
    pub async fn dispatch_response(&self, response: &aux_rpc::RpcResponse<'_>) {
        self.counters
            .fragments_received
            .fetch_add(1, Ordering::Relaxed);
        let mut pending = self.pending.lock().await;
        // Taken out of the map for the duration: a fragment that does not
        // complete the call is put back untouched.
        let Some(mut call) = pending.remove(&response.id) else {
            return;
        };

        // Ahead of everything, and deliberately ahead of `response_started`:
        // another drone answering this id says nothing about whether ours got
        // the request, so letting it stop the resend loop guarantees a timeout.
        if call
            .expected_sender
            .as_deref()
            .is_some_and(|expected| response.sender != expected.as_bytes())
        {
            self.counters
                .fragments_wrong_sender
                .fetch_add(1, Ordering::Relaxed);
            pending.insert(response.id, call);
            return;
        }

        // A fragment bearing this id from our drone proves the Request
        // datagram landed, so the resend loop must stop even if this
        // particular fragment turns out to be unusable.
        call.response_started = true;

        if call.assembly.is_none() {
            let bounded =
                response.total > 0 && response.total as usize <= aux_rpc::MAX_RESPONSE_FRAGMENTS;
            let seeded = bounded
                .then(|| aux_rpc::ResponseDecoder::new(response.oti))
                .flatten();
            let Some(decoder) = seeded else {
                // A fragment count or a transfer length we cannot bound, both
                // numbers the radio handed us. Complete the caller with an
                // honest gateway error rather than sizing a decoder from them.
                self.counters
                    .responses_out_of_range
                    .fetch_add(1, Ordering::Relaxed);
                self.counters.pending_now.fetch_sub(1, Ordering::Relaxed);
                let _ = call.sender.send(RpcResponseOwned {
                    status: 502,
                    body: b"relay response geometry out of range".to_vec(),
                    headers: Vec::new(),
                });
                return;
            };
            call.assembly = Some(Assembly {
                status: response.status,
                total: response.total,
                received: 0,
                accepted: 0,
                decoder,
            });
        }

        let index = response.index as usize;
        // `total` is capped at MAX_RESPONSE_FRAGMENTS (64), so every in-range
        // index owns a distinct bit; the mask exists only to keep the shift
        // defined for an out-of-range one, which `fresh` then rejects.
        let bit = 1u64 << (index % 64);
        let assembly = call.assembly.as_mut().expect("seeded above");
        // A stale fragment from a recycled id must not reach the decoder, an
        // out-of-range index must not be accepted, and the lane may re-deliver
        // so a duplicate must not be counted or decoded twice.
        let fresh = assembly.total == response.total
            && index < assembly.total as usize
            && assembly.accepted & bit == 0;
        let outcome = fresh.then(|| assembly.decoder.push(response.index, response.body));
        if matches!(outcome, Some(aux_rpc::FragmentOutcome::Pending)) {
            assembly.accepted |= bit;
            assembly.received += 1;
        }
        let status = assembly.status;

        match outcome {
            None => {
                self.counters
                    .fragments_duplicate
                    .fetch_add(1, Ordering::Relaxed);
                pending.insert(response.id, call);
            }
            Some(aux_rpc::FragmentOutcome::BadSymbol) => {
                self.counters
                    .fragments_bad_symbol
                    .fetch_add(1, Ordering::Relaxed);
                pending.insert(response.id, call);
            }
            Some(aux_rpc::FragmentOutcome::Pending) => {
                pending.insert(response.id, call);
            }
            Some(aux_rpc::FragmentOutcome::Complete(object)) => {
                self.counters.pending_now.fetch_sub(1, Ordering::Relaxed);
                // The reassembled bytes are the encoded object, not the body:
                // the drone folds an allow-listed header block in after the
                // body and marks it in the status's top bit. Split it back
                // apart here, so every caller of `call` sees a real HTTP
                // status, the body alone, and whatever headers crossed.
                let (status, body, headers) = aux_rpc::unpack_response(status, object);
                let _ = call.sender.send(RpcResponseOwned {
                    status,
                    body,
                    headers,
                });
            }
        }
    }
}
