use super::*;

/// Extract the early-reply `Value`, or panic if the parse produced anything
/// but a terminal reply.
fn reply(line: &[u8]) -> Value {
    match parse_command(line) {
        Parsed::Reply(v) => v,
        Parsed::Cmd(c) => panic!("expected an early reply, got command {c:?}"),
        Parsed::Subscribe => panic!("expected an early reply, got a subscribe"),
    }
}

/// Extract the apply-ready `Command`, or panic if the parse produced a reply
/// or a subscribe.
fn cmd(line: &[u8]) -> Command {
    match parse_command(line) {
        Parsed::Cmd(c) => c,
        Parsed::Reply(v) => panic!("expected a command, got reply {v}"),
        Parsed::Subscribe => panic!("expected a command, got a subscribe"),
    }
}

/// Assert the request parses to [`Parsed::Subscribe`].
fn subscribe(line: &[u8]) {
    assert!(
        matches!(parse_command(line), Parsed::Subscribe),
        "expected a subscribe request"
    );
}

#[test]
fn open_close_status_parse_to_commands() {
    assert_eq!(cmd(br#"{"op":"open"}"#), Command::Open);
    assert_eq!(cmd(br#"{"op":"close"}"#), Command::Close);
    assert_eq!(cmd(br#"{"op":"status"}"#), Command::Status);
}

#[test]
fn send_parses_its_aux_framed_payload() {
    // A `send` carries the already-aux-framed datagram bytes. Well-formed
    // sends round-trip the exact frame; a send without a frame is a clean
    // E_BAD_REQUEST, never a silent no-op or a panic.
    let frame: Vec<u8> = vec![
        0xAD, 0x02, 0x01, 0x08, 0x00, 0x05, b'h', b'e', b'l', b'l', b'o',
    ];
    let c = cmd(br#"{"op":"send","frame":[173,2,1,8,0,5,104,101,108,108,111]}"#);
    assert_eq!(c, Command::Send { frame });
    let v = reply(br#"{"op":"send"}"#);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"], "E_BAD_REQUEST: missing frame");
}

#[test]
fn subscribe_parses_to_the_streaming_request() {
    subscribe(br#"{"op":"subscribe"}"#);
}

#[test]
fn publish_parses_the_decoded_channel_and_payload() {
    // `publish` carries the DECODED inner payload, not an aux frame — the
    // consumer already stripped the header, and `subscribe` streams exactly
    // these bytes. A request missing either field is a clean E_BAD_REQUEST
    // naming which one, so the caller can fix its encoder.
    let c = cmd(br#"{"op":"publish","channel":8,"payload":[104,105]}"#);
    assert_eq!(
        c,
        Command::Publish {
            channel: 8,
            payload: vec![b'h', b'i']
        }
    );
    assert_eq!(
        reply(br#"{"op":"publish","payload":[1]}"#)["error"],
        "E_BAD_REQUEST: missing channel"
    );
    assert_eq!(
        reply(br#"{"op":"publish","channel":8}"#)["error"],
        "E_BAD_REQUEST: missing payload"
    );
}

#[tokio::test]
async fn a_published_datagram_reaches_every_subscriber() {
    // The end the aux-uplink consumer now feeds: it decodes off the port it
    // owns and hands the two application channels back here, so a plugin
    // holding a `subscribe` connection still receives inbound app traffic
    // even though this process no longer binds the receive port.
    let (tx, _keepalive) = broadcast::channel::<(u8, Vec<u8>)>(16);
    let mut plugin = tx.subscribe();
    let v = publish_app_datagram(true, &tx, AuxChannel::AppCommand as u8, b"pong".to_vec());
    assert_eq!(v["ok"], true);
    assert_eq!(
        v["delivered"], 2,
        "the reply counts the subscribers reached"
    );
    assert_eq!(
        plugin.try_recv().expect("the subscriber must receive it"),
        (AuxChannel::AppCommand as u8, b"pong".to_vec())
    );
}

#[test]
fn publish_refuses_every_channel_that_is_not_an_application_lane() {
    // The aux lane also carries MAVLink, relay RPC, link feedback and the
    // config tunnel, each with its own consumer in the router process. The
    // caller is another process across a socket, so this gate is the only
    // thing stopping a MAVLink frame from being delivered to a plugin as
    // application bytes it has no framing for.
    let (tx, _keepalive) = broadcast::channel::<(u8, Vec<u8>)>(16);
    for channel in [
        AuxChannel::Mavlink as u8,
        AuxChannel::Request as u8,
        AuxChannel::LinkFeedback as u8,
        AuxChannel::ConfigTunnel as u8,
    ] {
        let v = publish_app_datagram(true, &tx, channel, vec![0xFF]);
        assert_eq!(v["ok"], false, "channel {channel} must be refused");
        assert_eq!(v["error"], "E_BAD_CHANNEL");
    }
    // And both application channels are accepted.
    for channel in [AuxChannel::AppStream as u8, AuxChannel::AppCommand as u8] {
        assert_eq!(
            publish_app_datagram(true, &tx, channel, vec![1])["ok"],
            true
        );
    }
}

#[test]
fn publish_with_no_subscriber_is_success_not_a_drop() {
    // The normal state of a drone with no plugin attached. Reporting this as
    // a failure would make a healthy rig's caller count 100% loss on a lane
    // that is working exactly as designed.
    let (tx, _) = broadcast::channel::<(u8, Vec<u8>)>(16);
    let v = publish_app_datagram(true, &tx, AuxChannel::AppStream as u8, vec![7]);
    assert_eq!(v["ok"], true);
    assert_eq!(v["delivered"], 0);
}

#[test]
fn publish_is_refused_when_the_operator_disabled_the_aux_lane() {
    let (tx, mut sub) = broadcast::channel::<(u8, Vec<u8>)>(16);
    let v = publish_app_datagram(false, &tx, AuxChannel::AppStream as u8, vec![1]);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"], "E_AUX_DISABLED");
    assert!(
        sub.try_recv().is_err(),
        "a refused publish must not reach the fan-out"
    );
}

#[test]
fn send_is_refused_when_aux_is_disabled_or_the_pair_is_not_open() {
    // The dead-switch is checked before the datagram write: with
    // aux_enable=false a send refuses with E_AUX_DISABLED and never touches
    // the radio (the pure decision proves the short-circuit).
    let disabled = aux_disabled_reply(false).expect("disabled refuses the send");
    assert_eq!(disabled["ok"], false);
    assert_eq!(disabled["error"], "E_AUX_DISABLED");
    // With the dead-switch on the send proceeds to the open-state check
    // (which needs the live process group, covered on-rig).
    assert!(aux_disabled_reply(true).is_none());
}

#[test]
fn bad_json_is_rejected_before_any_radio_access() {
    // A malformed line never becomes a Command, so the service replies
    // without ever locking the process group (and never starts the aux pair).
    let v = reply(b"not json");
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().starts_with("E_BAD_REQUEST"));
}

#[test]
fn unknown_op_is_rejected() {
    let v = reply(br#"{"op":"frob"}"#);
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().starts_with("E_UNKNOWN_OP"));
}

#[test]
fn open_with_aux_disabled_is_refused_before_any_radio_access() {
    // The operator dead-switch: with aux_enable=false the open is refused
    // with the distinct E_AUX_DISABLED error and never reaches the radio
    // group, so no wfb_tx/wfb_rx process is spawned. (The apply path locks
    // the process group only AFTER this check, which this pure decision
    // proves is short-circuited.)
    let disabled = aux_disabled_reply(false).expect("disabled refuses the open");
    assert_eq!(disabled["ok"], false);
    assert_eq!(disabled["error"], "E_AUX_DISABLED");
    // With the dead-switch on, the open is allowed to proceed.
    assert!(aux_disabled_reply(true).is_none());
}

#[test]
fn an_empty_line_is_a_bad_request_not_a_panic() {
    // The framing strips the trailing newline before dispatch, so the handler
    // can hand an empty slice to the parser (EOF before any byte). It must be
    // a clean E_BAD_REQUEST, never a panic — and critically, never an open.
    let v = reply(b"");
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().starts_with("E_BAD_REQUEST"));
}

/// A ground-station state whose uplink egress points at a loopback socket
/// the test reads, standing in for the receive chain's aux `wfb_tx`.
async fn uplink(aux_enable: bool) -> (AuxCmdState, tokio::net::UdpSocket) {
    let wire = tokio::net::UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
    let port = wire.local_addr().unwrap().port();
    let (tx, _) = broadcast::channel(16);
    let state = AuxCmdState::ground(port, aux_enable, tx).await.unwrap();
    (state, wire)
}

fn send_cmd(channel: AuxChannel, payload: &[u8]) -> Command {
    Command::Send {
        frame: aux_mux::encode(channel, payload).unwrap(),
    }
}

/// Nothing arrived at the stand-in transmitter within a short window.
async fn assert_nothing_radiated(wire: &tokio::net::UdpSocket) {
    let mut buf = [0u8; 64];
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            wire.recv_from(&mut buf)
        )
        .await
        .is_err(),
        "nothing may reach the uplink"
    );
}

#[tokio::test]
async fn the_ground_uplink_sends_only_between_open_and_close() {
    // The drone contract, kept on the ground: a send before open (or after
    // close) is E_AUX_NOT_OPEN, and an open admits the send, which reaches
    // the uplink ingress as the identical aux frame.
    let (state, wire) = uplink(true).await;
    let port = wire.local_addr().unwrap().port();
    let v = apply(send_cmd(AuxChannel::AppCommand, b"go"), &state).await;
    assert_eq!(v["error"], "E_AUX_NOT_OPEN");
    assert_nothing_radiated(&wire).await;

    let v = apply(Command::Open, &state).await;
    assert_eq!(v, json!({"ok": true, "active": true, "tx_port": port}));
    assert_eq!(apply(Command::Status, &state).await["active"], true);

    let v = apply(send_cmd(AuxChannel::AppCommand, b"go"), &state).await;
    assert_eq!(v, json!({"ok": true}));
    let mut buf = [0u8; 64];
    let (n, _) = wire.recv_from(&mut buf).await.unwrap();
    assert_eq!(
        &buf[..n],
        aux_mux::encode(AuxChannel::AppCommand, b"go")
            .unwrap()
            .as_slice()
    );

    assert_eq!(
        apply(Command::Close, &state).await,
        json!({"ok": true, "active": false})
    );
    let v = apply(send_cmd(AuxChannel::AppStream, b"x"), &state).await;
    assert_eq!(v["error"], "E_AUX_NOT_OPEN");
}

#[tokio::test]
async fn the_ground_uplink_refuses_every_frame_that_is_not_application_traffic() {
    // The ground uplink also carries the drone's MAVLink, relay RPC, link
    // feedback and config tunnel. A socket client must never inject into
    // those planes, and a malformed frame must never be radiated.
    let (state, wire) = uplink(true).await;
    apply(Command::Open, &state).await;
    for channel in [
        AuxChannel::Mavlink,
        AuxChannel::Request,
        AuxChannel::LinkFeedback,
        AuxChannel::ConfigTunnel,
    ] {
        let v = apply(send_cmd(channel, &[0xFD, 0x09]), &state).await;
        assert_eq!(v["error"], "E_BAD_CHANNEL", "channel {}", channel as u8);
    }
    let mut truncated = aux_mux::encode(AuxChannel::AppStream, b"hello").unwrap();
    truncated.pop();
    let v = apply(Command::Send { frame: truncated }, &state).await;
    assert!(v["error"].as_str().unwrap().starts_with("E_BAD_FRAME"));
    let v = apply(
        Command::Send {
            frame: b"raw".to_vec(),
        },
        &state,
    )
    .await;
    assert!(v["error"].as_str().unwrap().starts_with("E_BAD_FRAME"));
    assert_nothing_radiated(&wire).await;
}

#[tokio::test]
async fn the_ground_uplink_honours_the_operator_dead_switch() {
    let (state, wire) = uplink(false).await;
    assert_eq!(
        apply(Command::Open, &state).await["error"],
        "E_AUX_DISABLED"
    );
    let v = apply(send_cmd(AuxChannel::AppCommand, b"go"), &state).await;
    assert_eq!(v["error"], "E_AUX_DISABLED");
    assert_eq!(
        state.publish(AuxChannel::AppStream as u8, vec![1])["error"],
        "E_AUX_DISABLED"
    );
    assert_nothing_radiated(&wire).await;
}

#[test]
fn the_drone_downlink_accepts_only_application_stream_frames() {
    // The downlink carries the vehicle's MAVLink, relay RPC responses and the
    // config tunnel; a socket client must not be able to forge any of them.
    for channel in [
        AuxChannel::Mavlink,
        AuxChannel::Request,
        AuxChannel::Response,
        AuxChannel::Status,
        AuxChannel::Identity,
        AuxChannel::LinkFeedback,
        AuxChannel::ConfigTunnel,
        AuxChannel::AppCommand,
    ] {
        let frame = aux_mux::encode(channel, &[0xFD, 0x09]).unwrap();
        assert_eq!(
            downlink_send_refusal(&frame).unwrap()["error"],
            "E_BAD_CHANNEL",
            "channel {}",
            channel as u8
        );
    }
    let mut truncated = aux_mux::encode(AuxChannel::AppStream, b"hello").unwrap();
    truncated.pop();
    assert!(downlink_send_refusal(&truncated).unwrap()["error"]
        .as_str()
        .unwrap()
        .starts_with("E_BAD_FRAME"));
    let ok = aux_mux::encode(AuxChannel::AppStream, b"hello").unwrap();
    assert_eq!(downlink_send_refusal(&ok), None);
}
