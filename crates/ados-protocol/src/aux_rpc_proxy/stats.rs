//! Proxy counter snapshot.

use super::*;

/// A point-in-time read of [`RpcProxyCounters`], shaped for the status route.
#[derive(Debug, Default, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct RpcProxyStats {
    pub calls_started: u64,
    pub calls_ok: u64,
    pub calls_timeout: u64,
    pub calls_incomplete: u64,
    pub calls_send_error: u64,
    pub calls_channel_closed: u64,
    pub calls_busy: u64,
    pub retransmits: u64,
    pub retransmit_send_errors: u64,
    pub fragments_received: u64,
    pub fragments_duplicate: u64,
    pub fragments_wrong_sender: u64,
    pub fragments_bad_symbol: u64,
    pub responses_out_of_range: u64,
    pub pending_now: u64,
}

impl AuxRpcProxy {
    /// A synchronous read of the lane's counters, for the status route.
    pub fn stats(&self) -> RpcProxyStats {
        let c = &self.counters;
        RpcProxyStats {
            calls_started: c.calls_started.load(Ordering::Relaxed),
            calls_ok: c.calls_ok.load(Ordering::Relaxed),
            calls_timeout: c.calls_timeout.load(Ordering::Relaxed),
            calls_incomplete: c.calls_incomplete.load(Ordering::Relaxed),
            calls_send_error: c.calls_send_error.load(Ordering::Relaxed),
            calls_channel_closed: c.calls_channel_closed.load(Ordering::Relaxed),
            calls_busy: c.calls_busy.load(Ordering::Relaxed),
            retransmits: c.retransmits.load(Ordering::Relaxed),
            retransmit_send_errors: c.retransmit_send_errors.load(Ordering::Relaxed),
            fragments_received: c.fragments_received.load(Ordering::Relaxed),
            fragments_duplicate: c.fragments_duplicate.load(Ordering::Relaxed),
            fragments_wrong_sender: c.fragments_wrong_sender.load(Ordering::Relaxed),
            fragments_bad_symbol: c.fragments_bad_symbol.load(Ordering::Relaxed),
            responses_out_of_range: c.responses_out_of_range.load(Ordering::Relaxed),
            pending_now: c.pending_now.load(Ordering::Relaxed),
        }
    }
}
