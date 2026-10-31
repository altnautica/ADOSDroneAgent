use super::*;

/// A peer that accepts a connection and then never reads.
///
/// This is the shape that stalls a read loop: the socket buffer fills, the
/// write blocks, and every drone behind that consumer goes quiet.
async fn deaf_listener(path: &std::path::Path) -> tokio::task::JoinHandle<()> {
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            match listener.accept().await {
                // Hold the connection open and never read from it.
                Ok((s, _)) => held.push(s),
                Err(_) => return,
            }
        }
    })
}

#[tokio::test]
async fn a_peer_that_stops_reading_cannot_stall_the_caller() {
    // The caller awaits this inside the aux consumer's single recv loop, so
    // an unbounded write to a wedged proxy stalls that slot's whole inbound
    // path — MAVLink ingest, status frames and the peer cache, not just RPC.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("ingest.sock");
    let _listener = deaf_listener(&sock).await;

    let ingest = AuxRpcResponseIngest::new(&sock);
    let big = vec![0u8; 512 * 1024];

    // Write until the buffer fills; every call must return promptly.
    let started = std::time::Instant::now();
    for _ in 0..8 {
        ingest.send(&big).await;
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "sends took {elapsed:?}; a wedged peer is stalling the caller"
    );
}

#[tokio::test]
async fn an_absent_listener_returns_promptly() {
    let ingest = AuxRpcResponseIngest::new(std::path::Path::new("/nonexistent/ados/ingest.sock"));
    let started = std::time::Instant::now();
    ingest.send(b"payload").await;
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn the_ipc_bounds_are_far_below_the_call_bound() {
    // These guard a read loop, not a call. A bound anywhere near the RPC
    // timeout would let one wedged proxy silence a drone for seconds.
    assert!(IPC_WRITE_TIMEOUT < RPC_DEFAULT_TIMEOUT / 10);
    assert!(IPC_CONNECT_TIMEOUT < RPC_DEFAULT_TIMEOUT / 10);
}
use crate::aux_mux;
use tokio::net::UdpSocket;
use tokio::time::Duration;

/// A no-radio egress that the test can read back from.
async fn loopback_egress() -> (
    AuxEgress,
    tokio::sync::mpsc::Receiver<(AuxChannel, Vec<u8>)>,
) {
    let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sock.connect(("127.0.0.1", port)).await.unwrap();
    let egress = AuxEgress::connected_for_test(sock);

    let (tx, rx) = tokio::sync::mpsc::channel::<(AuxChannel, Vec<u8>)>(8);
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while let Ok((n, _)) = listener.recv_from(&mut buf).await {
            if let Ok((ch, payload)) = aux_mux::decode(&buf[..n]) {
                let _ = tx.send((ch, payload.to_vec())).await;
            }
        }
    });
    (egress, rx)
}

/// The device id the tests address, and the one their drone answers with.
const DRONE: &[u8] = b"0a1b2c3d4e5f";

/// The encoded aux payloads for one response, exactly as a drone emits
/// them: RaptorQ symbols, each stamped with the sender's device id.
fn response_payloads(sender: &[u8], id: u32, status: u16, body: &[u8]) -> Vec<Vec<u8>> {
    let split = aux_rpc::split_response(body).unwrap();
    let total = split.symbols.len() as u16;
    split
        .symbols
        .iter()
        .enumerate()
        .map(|(index, symbol)| {
            aux_rpc::encode_response_fragment(
                sender,
                id,
                status,
                index as u16,
                total,
                split.oti,
                symbol,
            )
            .unwrap()
        })
        .collect()
}

/// Decode one encoded fragment and hand it to the proxy, the way the
/// response listener does.
async fn deliver(proxy: &AuxRpcProxy, payload: &[u8]) {
    let fragment = aux_rpc::decode_response(payload).unwrap();
    proxy.dispatch_response(&fragment).await;
}

/// Deliver every fragment of one response.
async fn deliver_body(proxy: &AuxRpcProxy, sender: &[u8], id: u32, status: u16, body: &[u8]) {
    for payload in response_payloads(sender, id, status, body) {
        deliver(proxy, &payload).await;
    }
}

/// Sniff the id of the single call the proxy currently has in flight.
async fn pending_id(proxy: &AuxRpcProxy) -> u32 {
    let pending = proxy.pending.lock().await;
    *pending.keys().next().expect("one pending caller")
}

#[tokio::test]
async fn a_call_round_trips_through_egress_and_a_dispatched_response() {
    let (egress, mut sent) = loopback_egress().await;
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(500));

    let proxy_for_consumer = proxy.clone();
    let consumer = tokio::spawn(async move {
        let (channel, payload) = sent.recv().await.expect("no request datagram");
        assert_eq!(channel, AuxChannel::Request);
        let fragment = aux_rpc::decode_request_fragment(&payload).unwrap();
        assert_eq!(fragment.total, 1, "a small request is one datagram");
        let aux_rpc::ReassemblyOutcome::Complete(request) =
            aux_rpc::RequestReassembler::new().push(&fragment, std::time::Instant::now())
        else {
            panic!("a one-fragment request completes on arrival");
        };
        assert_eq!(request.method, RpcMethod::Get);
        assert_eq!(request.target, DRONE);
        assert_eq!(request.path, b"/api/pairing/info");
        assert!(request.body.is_empty());

        let body = br#"{"device_id":"abc"}"#;
        deliver_body(&proxy_for_consumer, DRONE, request.id, 200, body).await;
    });

    let result = proxy
        .call(DRONE, RpcMethod::Get, b"/api/pairing/info", &[])
        .await
        .expect("call must succeed");

    consumer.await.unwrap();
    assert_eq!(result.status, 200);
    assert_eq!(result.body, br#"{"device_id":"abc"}"#);
}

#[tokio::test]
async fn a_multi_fragment_response_completes_on_its_systematic_symbols_alone() {
    let (egress, mut sent) = loopback_egress().await;
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(500));

    let body: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
    let expected = body.clone();
    let proxy_call = proxy.clone();
    let call = tokio::spawn(async move {
        proxy_call
            .call(DRONE, RpcMethod::Get, b"/api/services", &[])
            .await
    });

    let (_, payload) = sent.recv().await.expect("request datagram");
    let id = aux_rpc::decode_request_fragment(&payload).unwrap().id;
    let fragments = response_payloads(DRONE, id, 200, &body);
    assert_eq!(fragments.len(), 3 + aux_rpc::RPC_REPAIR_SYMBOLS as usize);
    // Only the systematic symbols. The repair symbols are the cushion, not
    // a prerequisite: a caller that waited for all seven would be back to
    // the geometric loss decay they exist to remove.
    for fragment in &fragments[..3] {
        deliver(&proxy, fragment).await;
    }

    let result = call.await.unwrap().expect("call must succeed");
    assert_eq!(result.status, 200);
    assert_eq!(result.body, expected);
}

#[tokio::test]
async fn fragments_delivered_out_of_order_reassemble_identically() {
    let (egress, mut sent) = loopback_egress().await;
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(500));

    let body: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
    let expected = body.clone();
    let proxy_call = proxy.clone();
    let call = tokio::spawn(async move {
        proxy_call
            .call(DRONE, RpcMethod::Get, b"/api/services", &[])
            .await
    });

    let (_, payload) = sent.recv().await.expect("request datagram");
    let id = aux_rpc::decode_request_fragment(&payload).unwrap().id;
    let mut fragments = response_payloads(DRONE, id, 200, &body);
    fragments.reverse();
    for fragment in &fragments {
        deliver(&proxy, fragment).await;
    }

    let result = call.await.unwrap().expect("call must succeed");
    assert_eq!(result.body, expected);
}

#[tokio::test]
async fn a_thirty_kilobyte_response_completes_after_losing_four_fragments() {
    // The measured `/api/status/full`. All-or-nothing reassembly of its 26
    // fragments at the measured 0.7% per-fragment loss succeeded 17/20 on
    // the bench; the four repair symbols are what remove that decay.
    let (egress, mut sent) = loopback_egress().await;
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_secs(2));

    let body: Vec<u8> = (0..29_339u32).map(|i| (i % 251) as u8).collect();
    let expected = body.clone();
    let proxy_call = proxy.clone();
    let call = tokio::spawn(async move {
        proxy_call
            .call(DRONE, RpcMethod::Get, b"/api/status/full", &[])
            .await
    });

    let (_, payload) = sent.recv().await.expect("request datagram");
    let id = aux_rpc::decode_request_fragment(&payload).unwrap().id;
    let mut fragments = response_payloads(DRONE, id, 200, &body);
    assert_eq!(fragments.len(), 30, "26 systematic + 4 repair");
    // Reordered by the lane, and four of them never arrive.
    fragments.rotate_left(7);
    fragments.truncate(26);
    for fragment in &fragments {
        deliver(&proxy, fragment).await;
    }

    let result = call.await.unwrap().expect("26 of 30 must be enough");
    assert_eq!(result.body, expected);
    assert_eq!(proxy.stats().calls_ok, 1);
}

#[tokio::test]
async fn a_fragment_from_another_drone_is_dropped_and_counted() {
    // 24 drones share one fleet key, so a request id is only unique per
    // sender. Splicing a second drone's symbols into this call's decoder
    // yields a cleanly-decoded body belonging to neither aircraft —
    // RaptorQ has no per-object checksum that would catch it.
    let (egress, mut sent) = loopback_egress().await;
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(300));

    let proxy_call = proxy.clone();
    let call =
        tokio::spawn(async move { proxy_call.call(DRONE, RpcMethod::Get, b"/api/x", &[]).await });

    let (_, payload) = sent.recv().await.expect("request datagram");
    let id = aux_rpc::decode_request_fragment(&payload).unwrap().id;
    let imposter = response_payloads(b"ados-elsewhere", id, 200, b"not-your-answer");
    for fragment in &imposter {
        deliver(&proxy, fragment).await;
    }

    let err = call.await.unwrap().unwrap_err();
    let stats = proxy.stats();
    assert_eq!(
        err,
        RpcError::Timeout,
        "an imposter must not complete a call"
    );
    assert_eq!(stats.fragments_wrong_sender, imposter.len() as u64);
    assert_eq!(stats.calls_ok, 0);
    assert!(
        stats.retransmits > 0,
        "another drone's answer proves nothing about ours, so the resend \
             loop must keep running"
    );
}

#[tokio::test]
async fn the_addressed_drone_still_completes_the_call_after_an_imposter() {
    // Dropping the wrong sender's fragments must not poison the pending
    // entry: the real answer, arriving second, still has to land.
    let (egress, mut sent) = loopback_egress().await;
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_secs(2));

    let proxy_call = proxy.clone();
    let call =
        tokio::spawn(async move { proxy_call.call(DRONE, RpcMethod::Get, b"/api/x", &[]).await });

    let (_, payload) = sent.recv().await.expect("request datagram");
    let id = aux_rpc::decode_request_fragment(&payload).unwrap().id;
    for fragment in &response_payloads(b"ados-elsewhere", id, 200, b"wrong-drone") {
        deliver(&proxy, fragment).await;
    }
    deliver_body(&proxy, DRONE, id, 200, b"right-drone").await;

    let result = call.await.unwrap().expect("the addressed drone answered");
    assert_eq!(result.body, b"right-drone");
}

#[tokio::test]
async fn a_broadcast_call_accepts_whichever_drone_answers() {
    // An empty target is a broadcast: there is no single sender to hold
    // the fragments to, and gating one would break the diagnostic call
    // that does not know a device id yet.
    let (egress, mut sent) = loopback_egress().await;
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(500));

    let proxy_call = proxy.clone();
    let call =
        tokio::spawn(async move { proxy_call.call(&[], RpcMethod::Get, b"/api/x", &[]).await });

    let (_, payload) = sent.recv().await.expect("request datagram");
    let id = aux_rpc::decode_request_fragment(&payload).unwrap().id;
    deliver_body(&proxy, b"ados-anyone", id, 200, b"answered").await;

    let result = call.await.unwrap().expect("call must succeed");
    assert_eq!(result.body, b"answered");
    assert_eq!(proxy.stats().fragments_wrong_sender, 0);
}

#[tokio::test]
async fn a_duplicate_fragment_does_not_double_count() {
    let (egress, mut sent) = loopback_egress().await;
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(400));

    let body: Vec<u8> = (0..2000u32).map(|i| (i % 251) as u8).collect();
    let expected = body.clone();
    let proxy_call = proxy.clone();
    let call =
        tokio::spawn(async move { proxy_call.call(DRONE, RpcMethod::Get, b"/api/x", &[]).await });

    let (_, payload) = sent.recv().await.expect("request datagram");
    let id = aux_rpc::decode_request_fragment(&payload).unwrap().id;
    let fragments = response_payloads(DRONE, id, 200, &body);
    // Symbol 0 twice, then 1: a naive counter would report two received
    // with symbol 1 still missing.
    for i in [0usize, 0, 1] {
        deliver(&proxy, &fragments[i]).await;
    }

    let result = call.await.unwrap().expect("call must succeed");
    assert_eq!(result.body, expected);
    assert_eq!(proxy.stats().fragments_duplicate, 1);
}

#[tokio::test]
async fn a_missing_fragment_yields_incomplete_not_timeout() {
    let (egress, mut sent) = loopback_egress().await;
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(120));

    let body: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
    let proxy_for_consumer = proxy.clone();
    let consumer = tokio::spawn(async move {
        let (_, payload) = sent.recv().await.expect("request datagram");
        let id = aux_rpc::decode_request_fragment(&payload).unwrap().id;
        let fragments = response_payloads(DRONE, id, 200, &body);
        deliver(&proxy_for_consumer, &fragments[0]).await;
    });

    let err = proxy
        .call(DRONE, RpcMethod::Get, b"/api/x", &[])
        .await
        .unwrap_err();
    consumer.await.unwrap();
    assert_eq!(
        err,
        RpcError::Incomplete {
            received: 1,
            total: 7
        }
    );
    assert_eq!(
        err.to_string(),
        "relay response incomplete: 1/7 fragments",
        "the operator must be able to tell a dropped fragment from silence"
    );
}

#[tokio::test]
async fn a_fragment_count_past_the_ceiling_completes_with_502() {
    let (egress, mut sent) = loopback_egress().await;
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(400));

    let proxy_for_consumer = proxy.clone();
    let consumer = tokio::spawn(async move {
        let (_, payload) = sent.recv().await.expect("request datagram");
        let id = aux_rpc::decode_request_fragment(&payload).unwrap().id;
        proxy_for_consumer
            .dispatch_response(&aux_rpc::RpcResponse {
                id,
                sender: DRONE,
                status: 200,
                index: 0,
                total: 65,
                oti: 2,
                body: b"aa",
            })
            .await;
    });

    let result = proxy
        .call(DRONE, RpcMethod::Get, b"/api/x", &[])
        .await
        .expect("the caller must be completed, not left hanging");
    consumer.await.unwrap();
    assert_eq!(result.status, 502);
    assert_eq!(result.body, b"relay response geometry out of range");
    assert_eq!(proxy.stats().responses_out_of_range, 1);
}

#[tokio::test]
async fn a_transfer_length_past_the_ceiling_completes_with_502() {
    // `oti` is a number the radio handed us and RaptorQ sizes its matrices
    // from it. A corrupt one must be refused, not allocated for.
    let (egress, mut sent) = loopback_egress().await;
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(400));

    let proxy_for_consumer = proxy.clone();
    let consumer = tokio::spawn(async move {
        let (_, payload) = sent.recv().await.expect("request datagram");
        let id = aux_rpc::decode_request_fragment(&payload).unwrap().id;
        proxy_for_consumer
            .dispatch_response(&aux_rpc::RpcResponse {
                id,
                sender: DRONE,
                status: 200,
                index: 0,
                total: 2,
                oti: u32::MAX,
                body: b"aa",
            })
            .await;
    });

    let result = proxy
        .call(DRONE, RpcMethod::Get, b"/api/x", &[])
        .await
        .expect("the caller must be completed, not left hanging");
    consumer.await.unwrap();
    assert_eq!(result.status, 502);
    assert_eq!(proxy.stats().responses_out_of_range, 1);
}

#[tokio::test]
async fn a_symbol_of_the_wrong_size_is_dropped_and_counted() {
    // Radio damage, or two ends on different builds. Feeding it to RaptorQ
    // would corrupt a matrix that assumes every symbol is one size.
    let (egress, mut sent) = loopback_egress().await;
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(200));

    let proxy_for_consumer = proxy.clone();
    let consumer = tokio::spawn(async move {
        let (_, payload) = sent.recv().await.expect("request datagram");
        let id = aux_rpc::decode_request_fragment(&payload).unwrap().id;
        proxy_for_consumer
            .dispatch_response(&aux_rpc::RpcResponse {
                id,
                sender: DRONE,
                status: 200,
                index: 0,
                total: 5,
                oti: 64,
                body: b"only-nine",
            })
            .await;
    });

    let err = proxy
        .call(DRONE, RpcMethod::Get, b"/api/x", &[])
        .await
        .unwrap_err();
    consumer.await.unwrap();
    assert_eq!(
        err,
        RpcError::Incomplete {
            received: 0,
            total: 5
        }
    );
    assert_eq!(proxy.stats().fragments_bad_symbol, 1);
}

#[tokio::test]
async fn a_call_times_out_when_no_response_arrives() {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sock.connect(("127.0.0.1", 9u16)).await.unwrap();
    let egress = AuxEgress::connected_for_test(sock);
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(80));

    let started = std::time::Instant::now();
    let err = proxy
        .call(&[], RpcMethod::Get, b"/api/status", &[])
        .await
        .unwrap_err();
    assert_eq!(err, RpcError::Timeout);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn a_call_returns_send_error_when_egress_fails() {
    let egress = AuxEgress::with_timeout("/nonexistent/aux-cmd.sock", Duration::from_millis(40));
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(200));
    let err = proxy
        .call(&[], RpcMethod::Get, b"/api/status", &[])
        .await
        .unwrap_err();
    assert!(
        matches!(err, RpcError::Send(_)),
        "expected Send, got {err:?}"
    );
}

#[tokio::test]
async fn a_body_past_the_relay_limit_is_refused_before_sending() {
    let (egress, _sent) = loopback_egress().await;
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(200));
    let big_body = vec![b'A'; aux_rpc::MAX_REQUEST_BODY + 1];
    let err = proxy
        .call(&[], RpcMethod::Post, b"/api/x", &big_body)
        .await
        .unwrap_err();
    assert_eq!(err, RpcError::TooLarge);
}

#[tokio::test]
async fn a_body_larger_than_one_frame_goes_out_as_several_fragments() {
    let (egress, mut sent) = loopback_egress().await;
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(500));
    let body = vec![b'B'; 4 * aux_mux::AUX_MAX_PAYLOAD];
    let expected = body.clone();
    let proxy_for_consumer = proxy.clone();
    let consumer = tokio::spawn(async move {
        let mut reassembler = aux_rpc::RequestReassembler::new();
        loop {
            let (_, payload) = sent.recv().await.expect("request fragment");
            let fragment = aux_rpc::decode_request_fragment(&payload).unwrap();
            assert!(fragment.total > 1);
            if let aux_rpc::ReassemblyOutcome::Complete(request) =
                reassembler.push(&fragment, std::time::Instant::now())
            {
                assert_eq!(request.method, RpcMethod::Patch);
                assert_eq!(request.body, expected);
                deliver_body(&proxy_for_consumer, DRONE, request.id, 200, b"ok").await;
                return;
            }
        }
    });
    let result = proxy
        .call(DRONE, RpcMethod::Patch, b"/api/plugins/p/x/upload", &body)
        .await
        .expect("call must succeed");
    consumer.await.unwrap();
    assert_eq!(result.status, 200);
}

#[tokio::test]
async fn a_response_for_an_unknown_id_is_a_noop() {
    let (egress, _sent) = loopback_egress().await;
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(50));
    for fragment in &response_payloads(DRONE, 9999, 200, b"orphan") {
        deliver(&proxy, fragment).await;
    }
    assert!(proxy.pending.lock().await.is_empty());
}

#[tokio::test]
async fn concurrent_calls_with_distinct_ids_match_their_own_responses() {
    let (egress, mut sent) = loopback_egress().await;
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(500));
    let proxy_consumer = proxy.clone();

    let consumer = tokio::spawn(async move {
        for _ in 0..3 {
            let (ch, payload) = sent.recv().await.expect("request datagram");
            assert_eq!(ch, AuxChannel::Request);
            let request = aux_rpc::decode_request_fragment(&payload).unwrap();
            let body = format!("{}", request.id).into_bytes();
            deliver_body(&proxy_consumer, DRONE, request.id, 200, &body).await;
        }
    });

    let p1 = tokio::spawn({
        let proxy = proxy.clone();
        async move { proxy.call(&[], RpcMethod::Get, b"/api/a", &[]).await }
    });
    let p2 = tokio::spawn({
        let proxy = proxy.clone();
        async move { proxy.call(&[], RpcMethod::Get, b"/api/b", &[]).await }
    });
    let p3 = tokio::spawn({
        let proxy = proxy.clone();
        async move { proxy.call(&[], RpcMethod::Get, b"/api/c", &[]).await }
    });

    let r1 = p1.await.unwrap().unwrap();
    let r2 = p2.await.unwrap().unwrap();
    let r3 = p3.await.unwrap().unwrap();
    consumer.await.unwrap();

    assert_ne!(r1.body, r2.body);
    assert_ne!(r2.body, r3.body);
    assert_eq!(r1.status, 200);
    assert_eq!(r2.status, 200);
    assert_eq!(r3.status, 200);
}

#[tokio::test]
async fn reset_drains_pending_callers_with_503() {
    let (egress, _sent) = loopback_egress().await;
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_secs(60));

    let proxy_call = proxy.clone();
    let call_handle =
        tokio::spawn(async move { proxy_call.call(&[], RpcMethod::Get, b"/api/x", &[]).await });

    tokio::time::sleep(Duration::from_millis(20)).await;
    proxy.reset().await;
    let result = tokio::time::timeout(Duration::from_millis(500), call_handle)
        .await
        .expect("reset must unblock the caller within the bound")
        .unwrap();
    assert_eq!(result.unwrap().status, 503);
}

#[tokio::test]
async fn the_ingest_writes_and_the_listener_dispatches() {
    // End-to-end: proxy LISTENS, ingest CONNECTS, a Response written by
    // the ingest arrives at the listener and is dispatched to the pending
    // caller. The call's Request goes into a loopback void (no consumer
    // reading it), but the Response arrives via the IPC seam.
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("aux-rpc-responses.sock");

    // A loopback egress whose Request datagrams nobody reads — the call
    // succeeds on the Response side, not the Request side.
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sock.connect(("127.0.0.1", 9u16)).await.unwrap();
    let egress = AuxEgress::connected_for_test(sock);
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(500));

    // Spawn the listener.
    let cancel = Arc::new(Notify::new());
    let listener_handle = proxy.spawn_response_listener(sock_path.clone(), cancel.clone());

    // Give the listener time to bind.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Start a call — it will send a Request into the void and wait.
    let proxy_call = proxy.clone();
    let call_handle = tokio::spawn(async move {
        proxy_call
            .call(&[], RpcMethod::Get, b"/api/pairing/info", &[])
            .await
    });

    // Give the call time to register its pending entry.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Write a matching Response via the ingest. The call's request id was
    // assigned by the proxy (starts at 1), so we sniff it from the
    // pending map.
    let id = pending_id(&proxy).await;

    let ingest = AuxRpcResponseIngest::new(&sock_path);
    for payload in response_payloads(DRONE, id, 200, br#"{"ok":true}"#) {
        ingest.send(&payload).await;
    }

    let result = tokio::time::timeout(Duration::from_secs(2), call_handle)
        .await
        .expect("the call must complete within the bound")
        .unwrap();
    assert_eq!(result.unwrap().status, 200);

    cancel.notify_waiters();
    let _ = listener_handle.await;
}

#[tokio::test]
async fn the_listener_recovers_after_a_bind_failure() {
    // A directory squats on the socket path, so the first bind fails. Once
    // it is gone the listener must bind on a later retry and accept.
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("aux-rpc-responses.sock");
    std::fs::create_dir(&sock_path).unwrap();

    let (egress, _sent) = loopback_egress().await;
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(200));
    let cancel = Arc::new(Notify::new());
    let listener_handle = proxy.spawn_response_listener(sock_path.clone(), cancel.clone());

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        sock_path.is_dir(),
        "the failed bind must not remove the directory"
    );
    std::fs::remove_dir(&sock_path).unwrap();

    let started = std::time::Instant::now();
    let connected = loop {
        if UnixStream::connect(&sock_path).await.is_ok() {
            break true;
        }
        if started.elapsed() > Duration::from_secs(5) {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(connected, "the listener did not recover within 5 s");

    cancel.notify_waiters();
    let _ = listener_handle.await;
}

#[tokio::test]
async fn an_undecodable_frame_does_not_poison_the_fragments_after_it() {
    // A 25-fragment response is 25 IPC frames. One damaged frame must cost
    // exactly that fragment, not every fragment behind it.
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("aux-rpc-responses.sock");

    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sock.connect(("127.0.0.1", 9u16)).await.unwrap();
    let egress = AuxEgress::connected_for_test(sock);
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_secs(2));

    let cancel = Arc::new(Notify::new());
    let listener_handle = proxy.spawn_response_listener(sock_path.clone(), cancel.clone());
    tokio::time::sleep(Duration::from_millis(20)).await;

    let proxy_call = proxy.clone();
    let call_handle =
        tokio::spawn(async move { proxy_call.call(&[], RpcMethod::Get, b"/api/x", &[]).await });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let id = pending_id(&proxy).await;

    let body: Vec<u8> = (0..2000u32).map(|i| (i % 251) as u8).collect();
    let fragments = response_payloads(DRONE, id, 200, &body);
    let ingest = AuxRpcResponseIngest::new(&sock_path);
    ingest.send(&fragments[0]).await;
    ingest.send(b"not-a-fragment").await;
    ingest.send(&fragments[1]).await;

    let result = tokio::time::timeout(Duration::from_secs(3), call_handle)
        .await
        .expect("the call must complete within the bound")
        .unwrap();
    assert_eq!(result.unwrap().body, body);

    cancel.notify_waiters();
    let _ = listener_handle.await;
}

#[tokio::test]
async fn a_lost_request_datagram_is_retransmitted_until_the_drone_answers() {
    let (egress, mut sent) = loopback_egress().await;
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_secs(3));

    let proxy_for_consumer = proxy.clone();
    let consumer = tokio::spawn(async move {
        // The half-duplex radio swallowed the first two attempts while the
        // drone was mid video burst. The third is the one that lands.
        let (_, first) = sent.recv().await.expect("first attempt");
        let (_, second) = sent.recv().await.expect("first retransmit");
        let (_, third) = sent.recv().await.expect("second retransmit");
        assert_eq!(first, second, "a retransmit must be the identical datagram");
        assert_eq!(second, third);
        let request = aux_rpc::decode_request_fragment(&third).unwrap();
        deliver_body(
            &proxy_for_consumer,
            DRONE,
            request.id,
            200,
            b"late-but-answered",
        )
        .await;
    });

    let result = proxy
        .call(DRONE, RpcMethod::Get, b"/api/version", &[])
        .await
        .expect("a retransmitted request must still complete");
    consumer.await.unwrap();
    assert_eq!(result.body, b"late-but-answered");
    assert!(
        proxy.stats().retransmits >= 2,
        "one datagram on a 20-40% loss lane is a coin flip; it must be resent"
    );
}

#[tokio::test]
async fn a_retransmit_that_could_not_be_sent_is_not_counted_as_one_that_was() {
    // The counter used to be taken before the send and the send's error
    // dropped, so a broken egress — a closed socket, a stopped transmitter
    // — read exactly like a healthy radio the far end simply was not
    // answering. That points a diagnosis at the wrong half of the link.
    // A UDP socket connected to an unbound loopback port: a send leaves,
    // and the kernel returns the ICMP port-unreachable on the next one. The
    // egress re-opens after each failure, so with the transmitter down the
    // retransmits alternate between one that leaves (a real retransmit)
    // and one that cannot (a send error).
    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let dead = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = dead.local_addr().unwrap();
    drop(dead);
    sock.connect(dead_addr).await.unwrap();
    let egress = AuxEgress::connected_for_test(sock);
    let proxy = AuxRpcProxy::new(egress);

    let r = tokio::time::timeout(
        RPC_DEFAULT_TIMEOUT + Duration::from_secs(2),
        proxy.call(&[], RpcMethod::Get, b"/api/pairing/info", &[]),
    )
    .await;
    assert!(r.is_ok(), "the call never terminated");

    let stats = proxy.stats();
    assert!(
        stats.retransmit_send_errors > 0,
        "a broken egress must be visible as a send failure, not as silence \
             from the far end: {stats:?}"
    );
}

#[tokio::test]
async fn a_response_that_has_started_arriving_stops_the_retransmits() {
    let (egress, mut sent) = loopback_egress().await;
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(600));

    let body: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
    let proxy_for_consumer = proxy.clone();
    let consumer = tokio::spawn(async move {
        let (_, payload) = sent.recv().await.expect("request datagram");
        let id = aux_rpc::decode_request_fragment(&payload).unwrap().id;
        // One symbol of seven lands; the rest never do. The call still
        // fails Incomplete, but the request demonstrably got through, so
        // resending it would only duplicate the drone's work.
        let fragments = response_payloads(DRONE, id, 200, &body);
        deliver(&proxy_for_consumer, &fragments[0]).await;
        tokio::time::sleep(Duration::from_millis(450)).await;
        sent.try_recv().is_ok()
    });

    let err = proxy
        .call(DRONE, RpcMethod::Get, b"/api/x", &[])
        .await
        .unwrap_err();
    let resent = consumer.await.unwrap();
    assert_eq!(
        err,
        RpcError::Incomplete {
            received: 1,
            total: 7
        }
    );
    assert!(!resent, "no second Request datagram may leave the ground");
    assert_eq!(proxy.stats().retransmits, 0);
}

#[tokio::test]
async fn a_saturated_proxy_sheds_load_instead_of_queueing_radio_work() {
    let (egress, _sent) = loopback_egress().await;
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(200));
    {
        let mut pending = proxy.pending.lock().await;
        for id in 0..MAX_PENDING_CALLS as u32 {
            let (tx, _rx) = oneshot::channel::<RpcResponseOwned>();
            pending.insert(
                id,
                PendingCall {
                    sender: tx,
                    expected_sender: None,
                    assembly: None,
                    response_started: false,
                },
            );
        }
    }

    let err = proxy
        .call(&[], RpcMethod::Get, b"/api/x", &[])
        .await
        .unwrap_err();
    assert_eq!(err, RpcError::Busy);
    assert_eq!(err.to_string(), "relay proxy has too many calls in flight");
    assert_eq!(proxy.stats().calls_busy, 1);
}

#[tokio::test]
async fn a_cancelled_call_does_not_leak_its_pending_entry() {
    // An axum handler whose client disconnects drops the call future
    // mid-flight. Post-await cleanup never runs on that path.
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sock.connect(("127.0.0.1", 9u16)).await.unwrap();
    let egress = AuxEgress::connected_for_test(sock);
    let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_secs(60));

    let proxy_call = proxy.clone();
    let handle =
        tokio::spawn(async move { proxy_call.call(&[], RpcMethod::Get, b"/api/x", &[]).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(proxy.pending.lock().await.len(), 1);

    handle.abort();
    // The guard schedules its removal rather than taking the async mutex
    // inline, so give the spawned cleanup a turn.
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        proxy.pending.lock().await.is_empty(),
        "a dropped call future must not leak its sender and assembly forever"
    );
    assert_eq!(proxy.stats().pending_now, 0);
}

#[tokio::test]
async fn the_request_id_seed_is_not_a_fixed_one() {
    // Two proxies standing in for two process starts: if both began at 1, a
    // stale in-flight response from before a restart would silently
    // complete a different new caller.
    let (egress_a, _a) = loopback_egress().await;
    let (egress_b, _b) = loopback_egress().await;
    let a = AuxRpcProxy::with_timeout(egress_a, Duration::from_millis(50));
    let b = AuxRpcProxy::with_timeout(egress_b, Duration::from_millis(50));
    assert_ne!(a.next_id.load(Ordering::Relaxed), 1);
    assert_ne!(b.next_id.load(Ordering::Relaxed), 1);
}
