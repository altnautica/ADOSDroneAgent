//! Outbound calls: fragmenting, retrying and pending-call bookkeeping.

use super::*;

impl AuxRpcProxy {
    /// Forward one HTTP-shaped request to the drone and await its response.
    ///
    /// `target` is the device id of the drone this call is for; an empty slice
    /// broadcasts to every linked drone. `path` is the absolute path on the
    /// drone's own HTTP API, including the leading slash and any query string
    /// (e.g. `/api/logs?limit=5`). `body` is the request body (empty for GET).
    /// The drone's HTTP server returns the status and body; the proxy does not
    /// interpret either.
    pub async fn call(
        &self,
        target: &[u8],
        method: RpcMethod,
        path: &[u8],
        body: &[u8],
    ) -> Result<RpcResponseOwned, RpcError> {
        self.call_request(
            target,
            method,
            &aux_rpc::RequestParts {
                path,
                body,
                ..Default::default()
            },
        )
        .await
    }

    /// As [`Self::call`], carrying every part of the request: its relay ticket
    /// and the caller's content type as well as the path and body.
    pub async fn call_request(
        &self,
        target: &[u8],
        method: RpcMethod,
        parts: &aux_rpc::RequestParts<'_>,
    ) -> Result<RpcResponseOwned, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.call_with_id(id, target, method, parts).await
    }

    /// Send every fragment of one request, paced so a multi-fragment burst
    /// does not overrun the transmitter's ingress.
    pub(super) async fn send_fragments(&self, fragments: &[Vec<u8>]) -> Result<(), String> {
        let last = fragments.len().saturating_sub(1);
        for (index, fragment) in fragments.iter().enumerate() {
            self.egress
                .send(AuxChannel::Request, fragment)
                .await
                .map_err(|e| e.to_string())?;
            if index < last {
                tokio::time::sleep(REQUEST_FRAGMENT_PACING).await;
            }
        }
        Ok(())
    }

    pub(super) async fn call_with_id(
        &self,
        id: u32,
        target: &[u8],
        method: RpcMethod,
        parts: &aux_rpc::RequestParts<'_>,
    ) -> Result<RpcResponseOwned, RpcError> {
        let fragments =
            aux_rpc::encode_request(method, id, target, parts).map_err(|e| match e {
                aux_rpc::RequestEncodeError::BodyTooLarge => RpcError::TooLarge,
                aux_rpc::RequestEncodeError::FieldTooLong => RpcError::Encode,
            })?;
        // An empty target is a broadcast, which any linked drone may answer,
        // so there is no single sender to hold its fragments to. A target that
        // is not UTF-8 is not a device id any drone will ever send back, and
        // the lossy conversion makes that mismatch — the safe direction.
        let expected_sender = (!target.is_empty()).then(|| {
            String::from_utf8_lossy(target)
                .into_owned()
                .into_boxed_str()
        });

        let (tx, rx) = oneshot::channel::<RpcResponseOwned>();
        {
            let mut pending = self.pending.lock().await;
            if pending.len() >= MAX_PENDING_CALLS {
                self.counters.calls_busy.fetch_add(1, Ordering::Relaxed);
                return Err(RpcError::Busy);
            }
            pending.insert(
                id,
                PendingCall {
                    sender: tx,
                    expected_sender,
                    assembly: None,
                    response_started: false,
                },
            );
        }
        self.counters.pending_now.fetch_add(1, Ordering::Relaxed);
        self.counters.calls_started.fetch_add(1, Ordering::Relaxed);

        // Armed from here on: every exit below either removes the entry itself
        // and disarms, or is a cancellation the guard has to clean up for us.
        let mut guard = PendingGuard {
            pending: Arc::clone(&self.pending),
            counters: Arc::clone(&self.counters),
            id,
            armed: true,
        };

        if let Err(e) = self.send_fragments(&fragments).await {
            guard.disarm();
            self.remove_pending(id).await;
            self.counters
                .calls_send_error
                .fetch_add(1, Ordering::Relaxed);
            return Err(RpcError::Send(e));
        }

        // One datagram on a lane that loses 20-40% of the uplink is a coin
        // flip, so resend on a growing schedule until either the response
        // starts arriving or the caller's bound elapses. The bound starts once
        // the request is fully on the air, so a large request's own send time
        // does not eat the drone's answer time.
        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut attempt = 0usize;
        tokio::pin!(rx);

        loop {
            let wake = match RPC_RETRY_DELAYS.get(attempt).copied() {
                Some(d) => (tokio::time::Instant::now() + d).min(deadline),
                None => deadline,
            };
            tokio::select! {
                biased;
                res = &mut rx => {
                    guard.disarm();
                    return match res {
                        Ok(resp) => {
                            self.counters.calls_ok.fetch_add(1, Ordering::Relaxed);
                            Ok(resp)
                        }
                        Err(_) => {
                            self.remove_pending(id).await;
                            self.counters
                                .calls_channel_closed
                                .fetch_add(1, Ordering::Relaxed);
                            Err(RpcError::ChannelClosed)
                        }
                    };
                }
                _ = tokio::time::sleep_until(wake) => {
                    if tokio::time::Instant::now() >= deadline {
                        guard.disarm();
                        // Some fragments in flight is a different fault from silence.
                        let partial = self.remove_pending(id).await;
                        return match partial.and_then(|p| p.assembly) {
                            Some(a) => {
                                self.counters
                                    .calls_incomplete
                                    .fetch_add(1, Ordering::Relaxed);
                                Err(RpcError::Incomplete {
                                    received: a.received,
                                    total: a.total,
                                })
                            }
                            None => {
                                self.counters.calls_timeout.fetch_add(1, Ordering::Relaxed);
                                Err(RpcError::Timeout)
                            }
                        };
                    }
                    attempt += 1;
                    let started = self
                        .pending
                        .lock()
                        .await
                        .get(&id)
                        .map(|p| p.response_started)
                        .unwrap_or(false);
                    if !started {
                        // Count what actually went out. The counter used to be
                        // taken before the send and the send's error dropped,
                        // so a broken egress — a closed socket, a stopped
                        // transmitter — read exactly like a healthy radio the
                        // far end simply was not answering, which is the wrong
                        // half of the link to go looking at.
                        match self.send_fragments(&fragments).await {
                            Ok(()) => {
                                self.counters.retransmits.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => {
                                self.counters
                                    .retransmit_send_errors
                                    .fetch_add(1, Ordering::Relaxed);
                                tracing::warn!(
                                    request_id = id,
                                    attempt,
                                    error = %e,
                                    "aux_rpc_retransmit_send_failed"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Remove one pending entry and keep `pending_now` honest. Every removal
    /// outside `dispatch_response` goes through here.
    pub(super) async fn remove_pending(&self, id: u32) -> Option<PendingCall> {
        let removed = self.pending.lock().await.remove(&id);
        if removed.is_some() {
            self.counters.pending_now.fetch_sub(1, Ordering::Relaxed);
        }
        removed
    }
}

impl PendingGuard {
    /// Called on any path that already removed the entry itself.
    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let pending = Arc::clone(&self.pending);
        let counters = Arc::clone(&self.counters);
        let id = self.id;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if pending.lock().await.remove(&id).is_some() {
                    counters.pending_now.fetch_sub(1, Ordering::Relaxed);
                }
            });
        }
    }
}
