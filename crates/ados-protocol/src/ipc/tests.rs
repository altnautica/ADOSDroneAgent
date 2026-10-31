use super::*;
use crate::frame::{encode_frame, MAVLINK_MAX_FRAME};

fn temp_sock(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let unique = format!("ados-ipc-test-{}-{}.sock", std::process::id(), name);
    p.push(unique);
    let _ = std::fs::remove_file(&p);
    p
}

#[tokio::test]
async fn clients_that_hang_up_are_pruned_without_a_broadcast() {
    // msp.sock never broadcasts on a MAVLink FC; every connect/close there
    // used to leave its handle and fds behind until the next broadcast.
    let path = temp_sock("hangup-prune");
    let (server, _inbound) = IpcBroadcast::bind(&path, 4, false, Some(8)).await.unwrap();
    for _ in 0..50 {
        let c = UnixStream::connect(&path).await.unwrap();
        drop(c);
    }
    // Let the accept loop register all 50 before polling the count.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    let mut count = server.client_count().await;
    while count != 0 && started.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(20)).await;
        count = server.client_count().await;
    }
    assert_eq!(count, 0, "hung-up clients must be pruned");
    let _ = std::fs::remove_file(&path);
}

/// A client source that fails a fixed number of times before delegating to
/// a real listener.
///
/// EMFILE is chosen because it is the error that actually happens: the agent
/// runs a dozen services against one fd table, and a burst of plugin or
/// proxy connections is the realistic way `accept()` fails on a flight
/// daemon. It cannot be induced in-process without lowering the whole
/// process's fd limit, which is why this seam exists.
struct FlakyAcceptor {
    listener: OperatorListener,
    failures_left: usize,
}

impl ClientSource for FlakyAcceptor {
    // Same reason as the trait: the accept loop is spawned, so the future
    // must be `Send`, which `async fn` in a trait cannot state.
    #[allow(clippy::manual_async_fn)]
    fn next_client(&mut self) -> impl Future<Output = io::Result<UnixStream>> + Send {
        async move {
            if self.failures_left > 0 {
                self.failures_left -= 1;
                // 24 == EMFILE on both Linux and macOS.
                return Err(io::Error::from_raw_os_error(24));
            }
            self.listener.accept().await.map(|(s, _addr)| s)
        }
    }
}

/// One transient `accept()` error must not close the socket for the life of
/// the process.
///
/// Why the `Err(_) => break` this replaces survived review: nothing observable
/// changes when the accept task exits. The socket is still bound and the
/// kernel still accepts into its backlog, so a client's `connect()` SUCCEEDS
/// and the client then hangs forever with no frames. Meanwhile
/// `client_count()` keeps reporting the clients that connected before the
/// error and `dropped_clients()` never ticks, so no counter, log line or
/// health probe anywhere can tell that the socket stopped accepting. Both
/// `mavlink.sock` and `state.sock` are bound through this one function, so a
/// single hiccup under fd pressure silently closed the FC command surface and
/// the telemetry snapshot to every future client until a manual restart.
///
/// The assertion is therefore on DELIVERY to a client accepted after the
/// errors, not on the loop's internal state — which is the only thing that
/// distinguishes the two behaviours from outside.
#[tokio::test]
async fn the_accept_loop_survives_an_accept_error_and_still_serves_the_next_client() {
    let path = temp_sock("acceptretry");
    let listener = bind_command_socket(&path, 0o660).unwrap();
    let (server, _inbound) = IpcBroadcast::from_source(
        path.clone(),
        FlakyAcceptor {
            listener,
            failures_left: 2,
        },
        256,
        false,
        None,
    );

    // A client that connects only AFTER the loop has already absorbed two
    // accept failures. Before the fix the loop had exited on the first one,
    // so this connect succeeded at the kernel (the socket is still bound and
    // its backlog still accepts) and then hung forever with no frames — the
    // failure mode that made this invisible.
    let mut client = connect_with_retry(&path, 200, Duration::from_millis(20))
        .await
        .unwrap();
    // Two backoffs plus registration.
    tokio::time::sleep(ACCEPT_RETRY_BACKOFF * 3).await;
    assert_eq!(server.client_count().await, 1);

    server
        .broadcast(
            encode_frame(b"post-emfile", MAVLINK_MAX_FRAME)
                .unwrap()
                .into(),
        )
        .await;
    let got = tokio::time::timeout(
        Duration::from_secs(2),
        read_length_prefixed(&mut client, MAVLINK_MAX_FRAME, false),
    )
    .await
    .expect("a frame must reach a client accepted after an accept error")
    .unwrap();
    assert_eq!(got.as_deref(), Some(&b"post-emfile"[..]));
}

#[tokio::test]
async fn broadcasts_framed_payloads_to_all_clients() {
    let path = temp_sock("bcast");
    let (server, _inbound) = IpcBroadcast::bind(&path, 256, false, None).await.unwrap();

    let mut c1 = connect_with_retry(&path, 10, Duration::from_millis(20))
        .await
        .unwrap();
    let mut c2 = connect_with_retry(&path, 10, Duration::from_millis(20))
        .await
        .unwrap();
    // Let the accept loop register both clients.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(server.client_count().await, 2);

    server
        .broadcast(encode_frame(b"hello", MAVLINK_MAX_FRAME).unwrap().into())
        .await;

    let f1 = read_length_prefixed(&mut c1, MAVLINK_MAX_FRAME, false)
        .await
        .unwrap();
    let f2 = read_length_prefixed(&mut c2, MAVLINK_MAX_FRAME, false)
        .await
        .unwrap();
    assert_eq!(f1.as_deref(), Some(&b"hello"[..]));
    assert_eq!(f2.as_deref(), Some(&b"hello"[..]));
}

#[tokio::test]
async fn replays_last_state_on_connect() {
    let path = temp_sock("laststate");
    let (server, _inbound) = IpcBroadcast::bind(&path, 32, true, None).await.unwrap();

    // Publish before any client connects.
    server
        .broadcast(Bytes::from_static(b"{\"armed\":false}\n"))
        .await;

    let mut client = connect_with_retry(&path, 10, Duration::from_millis(20))
        .await
        .unwrap();
    let line = read_newline_line(&mut client, crate::state::STATE_V2_MAX_FRAME)
        .await
        .unwrap();
    assert_eq!(line.as_deref(), Some(&b"{\"armed\":false}"[..]));
}

#[tokio::test]
async fn slow_client_is_dropped_fast_client_survives() {
    let path = temp_sock("slow");
    // Small queue so a non-draining client fills it once the kernel socket
    // buffer is also full.
    let (server, _inbound) = IpcBroadcast::bind(&path, 2, false, None).await.unwrap();

    // "slow" connects but never reads; "fast" connects and drains continuously.
    let _slow = connect_with_retry(&path, 10, Duration::from_millis(20))
        .await
        .unwrap();
    let mut fast = connect_with_retry(&path, 10, Duration::from_millis(20))
        .await
        .unwrap();
    let fast_reader = tokio::spawn(async move {
        while let Ok(Some(_)) = read_length_prefixed(&mut fast, MAVLINK_MAX_FRAME, false).await {}
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(server.client_count().await, 2);

    // Large payloads fill the slow client's kernel send buffer; its writer
    // then blocks, its queue fills to depth 2, and the next broadcast drops
    // it. The fast client keeps draining so it never fills.
    let big = vec![0xABu8; 60_000];
    for _ in 0..60u32 {
        server
            .broadcast(encode_frame(&big, MAVLINK_MAX_FRAME).unwrap().into())
            .await;
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The slow client has been pruned; the fast client remains.
    assert_eq!(server.client_count().await, 1);
    // The eviction is observable: the drop counter advanced by exactly the
    // one stalled consumer, not the fast one that kept draining.
    assert_eq!(server.dropped_clients(), 1);
    fast_reader.abort();
}

#[tokio::test]
async fn dropped_client_counter_advances_when_a_consumer_stalls() {
    let path = temp_sock("dropcount");
    // Depth-2 queue so a non-draining client is evicted quickly once its
    // kernel send buffer and then its queue fill.
    let (server, _inbound) = IpcBroadcast::bind(&path, 2, false, None).await.unwrap();

    // A counter starts at zero before any client exists.
    assert_eq!(server.dropped_clients(), 0);

    // One consumer connects and then never reads a single byte.
    let _stalled = connect_with_retry(&path, 10, Duration::from_millis(20))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(server.client_count().await, 1);

    // Push large payloads until the stalled consumer's kernel buffer and
    // queue fill and the next broadcast evicts it.
    let big = vec![0x5Au8; 60_000];
    for _ in 0..60u32 {
        server
            .broadcast(encode_frame(&big, MAVLINK_MAX_FRAME).unwrap().into())
            .await;
        tokio::time::sleep(Duration::from_millis(2)).await;
        if server.client_count().await == 0 {
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The stalled consumer was pruned, and the eviction is no longer
    // silent: the producer-visible counter advanced.
    assert_eq!(server.client_count().await, 0);
    assert_eq!(server.dropped_clients(), 1);

    // The counter is monotonic: it does not reset across further broadcasts
    // once the client is gone (no remaining client to drop).
    server
        .broadcast(encode_frame(b"tail", MAVLINK_MAX_FRAME).unwrap().into())
        .await;
    assert_eq!(server.dropped_clients(), 1);
}

#[tokio::test]
async fn a_clean_disconnect_is_not_counted_as_a_slow_client() {
    // The state-socket shape: replay the last buffer on connect, which is
    // what a short-lived reader comes for.
    let path = temp_sock("cleandisconnect");
    let (server, _inbound) = IpcBroadcast::bind(&path, 32, true, None).await.unwrap();
    server
        .broadcast(encode_frame(b"snapshot", MAVLINK_MAX_FRAME).unwrap().into())
        .await;

    // This is exactly the `ados-cloud` enrichment pattern: connect, take the
    // replayed snapshot, disconnect. Repeated at ~1 Hz it used to log an
    // `ipc_slow_client_evicted` every second on a perfectly healthy agent.
    for _ in 0..3u32 {
        let client = connect_with_retry(&path, 10, Duration::from_millis(20))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(client);
        tokio::time::sleep(Duration::from_millis(30)).await;

        // Two broadcasts: the first may still find the queue writable
        // before the writer task notices the peer is gone, the second is
        // guaranteed to meet a closed channel.
        for _ in 0..2u32 {
            server
                .broadcast(encode_frame(b"tick", MAVLINK_MAX_FRAME).unwrap().into())
                .await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    // The departed clients are pruned...
    assert_eq!(server.client_count().await, 0);
    // ...but none of them was slow, so the eviction counter must be
    // untouched. Before the Full/Closed split this read 3.
    assert_eq!(server.dropped_clients(), 0);
}

#[tokio::test]
async fn inbound_frames_from_client_reach_the_channel() {
    let path = temp_sock("inbound");
    let (_server, inbound) = IpcBroadcast::bind(&path, 256, false, Some(16))
        .await
        .unwrap();
    let mut inbound = inbound.expect("inbound channel requested");

    let mut client = connect_with_retry(&path, 10, Duration::from_millis(20))
        .await
        .unwrap();
    // Client writes a framed command toward the FC.
    let framed = encode_frame(b"command", MAVLINK_MAX_FRAME).unwrap();
    client.write_all(&framed).await.unwrap();
    client.flush().await.unwrap();

    let got = tokio::time::timeout(Duration::from_millis(500), inbound.recv())
        .await
        .unwrap()
        .expect("a command");
    assert_eq!(got.payload, b"command");
    // The kernel answers for a local peer, so the writing process is
    // identified rather than assumed.
    assert!(
        got.peer.uid.is_some(),
        "the accepting socket must record who wrote the payload"
    );
    assert!(!got.peer.off_box_source);
}

#[tokio::test]
async fn a_declared_off_box_source_sticks_and_is_not_forwarded() {
    let path = temp_sock("offbox");
    let (_server, inbound) = IpcBroadcast::bind(&path, 256, false, Some(16))
        .await
        .unwrap();
    let mut inbound = inbound.expect("inbound channel requested");

    let mut client = connect_with_retry(&path, 10, Duration::from_millis(20))
        .await
        .unwrap();
    for payload in [IPC_DECLARE_OFF_BOX_SOURCE, b"first", b"second"] {
        let framed = encode_frame(payload, MAVLINK_MAX_FRAME).unwrap();
        client.write_all(&framed).await.unwrap();
    }
    client.flush().await.unwrap();

    // The declaration itself never reaches the consumer, and both payloads
    // behind it are marked for the whole life of the connection.
    for expected in [&b"first"[..], &b"second"[..]] {
        let got = tokio::time::timeout(Duration::from_millis(500), inbound.recv())
            .await
            .unwrap()
            .expect("a command");
        assert_eq!(got.payload, expected);
        assert!(
            got.peer.off_box_source,
            "a declared forwarder must not read as a local producer"
        );
    }
}

#[test]
fn encode_injector_declaration_round_trips_through_the_parser() {
    // The producer's encoder and the router's parser must agree byte-for-byte,
    // with and without a ticket — a drift here silently un-arms the gate.
    for (id, ticket) in [("ai-mission", Some("abc.def")), ("autopilot-1", None)] {
        let frame = encode_injector_declaration(id, ticket);
        assert!(frame.starts_with(IPC_DECLARE_INJECTOR_PREFIX));
        assert_eq!(
            parse_injector_declaration(&frame),
            Some(InjectorClaim {
                client_id: id.into(),
                ticket: ticket.map(str::to_string),
            })
        );
    }
}

#[test]
fn parse_injector_declaration_reads_id_and_optional_ticket() {
    // id only
    let mut d = IPC_DECLARE_INJECTOR_PREFIX.to_vec();
    d.extend_from_slice(b"ai-mission");
    assert_eq!(
        parse_injector_declaration(&d),
        Some(InjectorClaim {
            client_id: "ai-mission".into(),
            ticket: None
        })
    );
    // id + ticket
    let mut d2 = IPC_DECLARE_INJECTOR_PREFIX.to_vec();
    d2.extend_from_slice(b"ai-mission;ticket=abc.def");
    assert_eq!(
        parse_injector_declaration(&d2),
        Some(InjectorClaim {
            client_id: "ai-mission".into(),
            ticket: Some("abc.def".into())
        })
    );
    // an empty client id is not a claim
    let mut empty = IPC_DECLARE_INJECTOR_PREFIX.to_vec();
    empty.extend_from_slice(b";ticket=abc");
    assert_eq!(parse_injector_declaration(&empty), None);
    // an unrelated payload is not an injector declaration
    assert_eq!(parse_injector_declaration(b"command"), None);
    assert_eq!(parse_injector_declaration(IPC_DECLARE_OFF_BOX_SOURCE), None);
}

#[tokio::test]
async fn a_declared_injector_sticks_and_is_not_forwarded() {
    let path = temp_sock("injector");
    let (_server, inbound) = IpcBroadcast::bind(&path, 256, false, Some(16))
        .await
        .unwrap();
    let mut inbound = inbound.expect("inbound channel requested");

    let mut client = connect_with_retry(&path, 10, Duration::from_millis(20))
        .await
        .unwrap();
    let mut decl = IPC_DECLARE_INJECTOR_PREFIX.to_vec();
    decl.extend_from_slice(b"ai-mission;ticket=tkt");
    for payload in [decl.as_slice(), b"first", b"second"] {
        let framed = encode_frame(payload, MAVLINK_MAX_FRAME).unwrap();
        client.write_all(&framed).await.unwrap();
    }
    client.flush().await.unwrap();

    // The declaration never reaches the FC, and both payloads behind it carry
    // the sticky injector claim.
    for expected in [&b"first"[..], &b"second"[..]] {
        let got = tokio::time::timeout(Duration::from_millis(500), inbound.recv())
            .await
            .unwrap()
            .expect("a command");
        assert_eq!(got.payload, expected);
        let claim = got.peer.injector.expect("a declared injector claim");
        assert_eq!(claim.client_id, "ai-mission");
        assert_eq!(claim.ticket.as_deref(), Some("tkt"));
    }
}

#[tokio::test]
async fn one_connection_declaring_does_not_mark_another() {
    let path = temp_sock("offbox-scope");
    let (_server, inbound) = IpcBroadcast::bind(&path, 256, false, Some(16))
        .await
        .unwrap();
    let mut inbound = inbound.expect("inbound channel requested");

    let mut declaring = connect_with_retry(&path, 10, Duration::from_millis(20))
        .await
        .unwrap();
    declaring
        .write_all(&encode_frame(IPC_DECLARE_OFF_BOX_SOURCE, MAVLINK_MAX_FRAME).unwrap())
        .await
        .unwrap();
    declaring.flush().await.unwrap();

    let mut plain = connect_with_retry(&path, 10, Duration::from_millis(20))
        .await
        .unwrap();
    plain
        .write_all(&encode_frame(b"local", MAVLINK_MAX_FRAME).unwrap())
        .await
        .unwrap();
    plain.flush().await.unwrap();

    let got = tokio::time::timeout(Duration::from_millis(500), inbound.recv())
        .await
        .unwrap()
        .expect("a command");
    assert_eq!(got.payload, b"local");
    assert!(!got.peer.off_box_source);
}

#[tokio::test]
async fn read_newline_line_caps_an_endless_line() {
    // A peer that never sends a newline must not grow the buffer past the
    // cap: the reader returns InvalidData instead of allocating unbounded.
    let (mut a, mut b) = UnixStream::pair().unwrap();
    let writer = tokio::spawn(async move {
        // Send more bytes than the cap, with no newline.
        let chunk = vec![b'x'; 64];
        for _ in 0..10 {
            if a.write_all(&chunk).await.is_err() {
                break;
            }
            let _ = a.flush().await;
        }
        // Keep the stream open so the reader hits the cap, not EOF.
        tokio::time::sleep(Duration::from_secs(2)).await;
    });

    let err = read_newline_line(&mut b, 128).await.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    writer.abort();
}

#[tokio::test]
async fn bind_command_socket_removes_stale_and_sets_mode() {
    let path = temp_sock("bindhelper");
    // Leave a stale regular file where the socket will go; bind must remove
    // it rather than fail with EADDRINUSE.
    std::fs::write(&path, b"stale").unwrap();
    assert!(path.exists());

    let listener = bind_command_socket(&path, 0o660).unwrap();

    // The requested permission bits are set on the bound socket.
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o660);

    drop(listener);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn a_broadcast_socket_is_never_world_accessible() {
    // Regression guard. These sockets were bound 0o666 to match an older
    // Python server, and two of them — the MAVLink and MSP lanes — forward
    // inbound bytes straight to the flight controller, so world-write meant
    // any local process could arm an aircraft. The trusted local plane is
    // root plus the operator group and nothing wider.
    let path = temp_sock("bcast-perm");
    let (server, _inbound) = IpcBroadcast::bind(&path, 8, false, None).await.unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(
        mode & 0o007,
        0,
        "broadcast socket must carry no world bits, got {:o}",
        mode & 0o777
    );
    assert_eq!(mode & 0o777, 0o660, "expected owner+group rw only");

    drop(server);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn serve_rpc_round_trips_a_request_and_closes_over_cap() {
    let path = temp_sock("rpc");
    let listener = bind_command_socket(&path, 0o660).unwrap();
    // Echo handler: prefix the request so the test sees the request bytes
    // (trailing newline stripped) reached the dispatch closure.
    let server = tokio::spawn(serve_rpc(listener, 16, |req: Vec<u8>| async move {
        let mut resp = b"echo:".to_vec();
        resp.extend_from_slice(&req);
        resp
    }));

    // A well-formed one-shot request gets one newline-terminated response.
    let mut client = connect_with_retry(&path, 10, Duration::from_millis(20))
        .await
        .unwrap();
    client.write_all(b"ping\n").await.unwrap();
    client.flush().await.unwrap();
    let line = read_newline_line(&mut client, 256).await.unwrap();
    assert_eq!(line.as_deref(), Some(&b"echo:ping"[..]));
    drop(client);

    // A request longer than the cap (16) with no newline closes the
    // connection with no response. The client then reads no reply: on
    // some platforms that is a clean EOF (`Ok(None)`), on others the
    // server closing with unread data resets the connection (`Err`).
    // Both mean "closed without a reply".
    let mut over = connect_with_retry(&path, 10, Duration::from_millis(20))
        .await
        .unwrap();
    let _ = over.write_all(&[b'x'; 64]).await;
    let _ = over.flush().await;
    let outcome = read_newline_line(&mut over, 256).await;
    assert!(
        matches!(outcome, Ok(None) | Err(_)),
        "over-cap request must get no reply, got {outcome:?}"
    );

    server.abort();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn plugin_plane_peers_are_injectors_whatever_they_declare() {
    let plugin_gid = Some(980);
    let legacy_user = Some(995);
    // A per-plugin user: primary group is the plugin group.
    assert!(is_plugin_peer(1201, &[980], legacy_user, plugin_gid));
    // The plugin group as a supplementary group counts too.
    assert!(is_plugin_peer(1201, &[1201, 980], legacy_user, plugin_gid));
    // The legacy shared plugin user, whatever its groups.
    assert!(is_plugin_peer(995, &[995], legacy_user, plugin_gid));
    // Root and an operator are not.
    assert!(!is_plugin_peer(0, &[0], legacy_user, plugin_gid));
    assert!(!is_plugin_peer(1000, &[1000, 990], legacy_user, plugin_gid));
    // Neither the user nor the group exists on this host: nobody matches.
    assert!(!is_plugin_peer(1201, &[980], None, None));
    // The claim a plugin peer starts with can never verify on a paired node.
    assert_eq!(plugin_plane_claim(1201).ticket, None);
}

#[test]
fn operator_peer_policy_admits_root_the_server_uid_and_operator_members_only() {
    let operator_gid = Some(990);
    // A plugin process: uid 1000, primary group 1000, no operator group.
    assert!(!operator_peer_allowed(1000, &[1000], 0, operator_gid));
    // The same uid with the operator group as a supplementary group.
    assert!(operator_peer_allowed(1000, &[1000, 990], 0, operator_gid));
    // The operator group as the primary group counts too.
    assert!(operator_peer_allowed(1000, &[990], 0, operator_gid));
    // Root and the serving process's own uid are always admitted.
    assert!(operator_peer_allowed(0, &[0], 0, operator_gid));
    assert!(operator_peer_allowed(501, &[20], 501, operator_gid));
    // With the operator group absent from the host nobody else gets in,
    // whatever groups it carries.
    assert!(!operator_peer_allowed(1000, &[1000, 990], 0, None));
}

#[tokio::test]
async fn a_refused_peer_is_closed_before_its_request_is_read() {
    // Root is admitted by rule, so this can only be observed as non-root.
    if nix::unistd::geteuid().is_root() {
        return;
    }
    let path = temp_sock("refused");
    let mut listener = bind_command_socket(&path, 0o660).unwrap();
    // Serve as a different uid with no operator group on the host: the
    // test's own uid is then an outsider, exactly like a plugin process.
    listener.server_uid = u32::MAX - 1;
    listener.operator_gid = None;
    let reached = Arc::new(AtomicU64::new(0));
    let counter = reached.clone();
    let server = tokio::spawn(serve_rpc(listener, 64, move |_req: Vec<u8>| {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::Relaxed);
            b"served".to_vec()
        }
    }));

    let mut client = connect_with_retry(&path, 10, Duration::from_millis(20))
        .await
        .unwrap();
    let _ = client.write_all(b"ping\n").await;
    let _ = client.flush().await;
    let outcome = tokio::time::timeout(Duration::from_secs(2), read_newline_line(&mut client, 256))
        .await
        .expect("a refused peer must be closed, not left hanging");
    assert!(
        matches!(outcome, Ok(None) | Err(_)),
        "a refused peer must get no reply, got {outcome:?}"
    );
    assert_eq!(
        reached.load(Ordering::Relaxed),
        0,
        "the handler must never run"
    );

    server.abort();
    let _ = std::fs::remove_file(&path);
}
