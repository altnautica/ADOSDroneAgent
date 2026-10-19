use super::*;
use crate::sources::{ChannelSourceMode, MAX_INJECT_TTL, MIN_INJECT_TTL};

fn state() -> CmdState {
    state_with(ChannelSourceMode::Inject)
}

fn state_with(mode: ChannelSourceMode) -> CmdState {
    CmdState {
        merge: Arc::new(Mutex::new(SourceMerge::new(mode))),
        latest_status: Arc::new(Mutex::new(Value::Null)),
        oob: Arc::new(OobQueue::default()),
        // An unpaired node: no key exists, so a claimed name stands, which
        // is the posture every native surface takes while unclaimed.
        injector_auth: Arc::new(InjectorAuth::new(std::path::PathBuf::from(
            "/nonexistent/pairing.json",
        ))),
        modules: Arc::new(single_pilot()),
    }
}

/// A lane with one connected pilot module and no autonomy module.
fn single_pilot() -> ModuleTargets {
    ModuleTargets {
        pilot_id: None,
        pilot_connected: Arc::new(AtomicBool::new(true)),
        autonomy: Vec::new(),
    }
}

#[test]
fn parse_rejects_malformed_json() {
    match parse_command(b"not json") {
        Parsed::Reply(v) => {
            assert_eq!(v["ok"], false);
            assert!(v["error"].as_str().unwrap().starts_with("E_BAD_REQUEST"));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn parse_rejects_unknown_op_and_missing_fields() {
    match parse_command(br#"{"op":"reboot"}"#) {
        Parsed::Reply(v) => assert_eq!(v["error"], "E_UNKNOWN_OP: reboot"),
        other => panic!("unexpected {other:?}"),
    }
    match parse_command(br#"{"op":"set_channels"}"#) {
        Parsed::Reply(v) => assert_eq!(v["error"], "E_MISSING_CHANNELS"),
        other => panic!("unexpected {other:?}"),
    }
    match parse_command(br#"{"op":"set_channel","index":1}"#) {
        Parsed::Reply(v) => assert_eq!(v["error"], "E_MISSING_VALUE"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn parse_rejects_wrong_count_and_out_of_range_values() {
    match parse_command(br#"{"op":"set_channels","channels":[992, 992]}"#) {
        Parsed::Reply(v) => assert_eq!(v["error"], "E_BAD_CHANNEL_COUNT"),
        other => panic!("unexpected {other:?}"),
    }
    let mut chans = vec![992u16; 16];
    chans[3] = 2000;
    let line = serde_json::to_vec(&json!({"op":"set_channels","channels":chans})).unwrap();
    match parse_command(&line) {
        Parsed::Reply(v) => assert_eq!(v["error"], "E_BAD_CHANNEL_VALUE: 2000"),
        other => panic!("unexpected {other:?}"),
    }
    match parse_command(br#"{"op":"set_channel","index":16,"value":992}"#) {
        Parsed::Reply(v) => assert_eq!(v["error"], "E_BAD_CHANNEL_INDEX: 16"),
        other => panic!("unexpected {other:?}"),
    }
    match parse_command(br#"{"op":"set_channel","index":0,"value":171}"#) {
        Parsed::Reply(v) => assert_eq!(v["error"], "E_BAD_CHANNEL_VALUE: 171"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn parse_accepts_valid_commands_with_ttl_and_client() {
    assert_eq!(
        parse_command(br#"{"op":"status"}"#),
        Parsed::Cmd(Command::Status)
    );
    assert_eq!(
        parse_command(
            br#"{"op":"set_channel","index":4,"value":1811,"ttl_ms":700,"client_id":"ai"}"#
        ),
        Parsed::Cmd(Command::SetChannel {
            index: 4,
            value: 1811,
            ttl: Duration::from_millis(700),
            claim: InjectorClaim {
                client_id: Some("ai".to_string()),
                ticket: None,
            },
            module: None,
        })
    );
    // Absent ttl defaults; out-of-window ttl clamps at parse time.
    match parse_command(br#"{"op":"set_channel","index":0,"value":992}"#) {
        Parsed::Cmd(Command::SetChannel { ttl, .. }) => {
            assert_eq!(ttl, DEFAULT_INJECT_TTL);
        }
        other => panic!("unexpected {other:?}"),
    }
    match parse_command(br#"{"op":"set_channel","index":0,"value":992,"ttl_ms":1}"#) {
        Parsed::Cmd(Command::SetChannel { ttl, .. }) => assert_eq!(ttl, MIN_INJECT_TTL),
        other => panic!("unexpected {other:?}"),
    }
    match parse_command(br#"{"op":"set_channel","index":0,"value":992,"ttl_ms":999999}"#) {
        Parsed::Cmd(Command::SetChannel { ttl, .. }) => assert_eq!(ttl, MAX_INJECT_TTL),
        other => panic!("unexpected {other:?}"),
    }
    let line =
        serde_json::to_vec(&json!({"op":"set_channels","channels":vec![992u16;16]})).unwrap();
    match parse_command(&line) {
        Parsed::Cmd(Command::SetChannels {
            channels,
            ttl,
            claim,
            module,
        }) => {
            assert_eq!(module, None);
            assert_eq!(channels, [992u16; 16]);
            assert_eq!(ttl, DEFAULT_INJECT_TTL);
            assert_eq!(claim, InjectorClaim::default());
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn apply_set_channels_updates_the_merge() {
    let st = state();
    let resp = dispatch(
        &serde_json::to_vec(&json!({"op":"set_channels","channels":vec![1000u16;16]})).unwrap(),
        &st,
    )
    .await;
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["channel_source"], "inject");
    assert_eq!(resp["authority"], "inject");
    assert_eq!(resp["ttl_ms"], DEFAULT_INJECT_TTL.as_millis() as u64);
    let (values, src) = st.merge.lock().await.current(Instant::now()).unwrap();
    assert_eq!(values, [1000u16; 16]);
    assert_eq!(src.as_str(), "inject");
}

#[tokio::test]
async fn apply_set_channel_updates_one_slot() {
    let st = state();
    let resp = dispatch(br#"{"op":"set_channel","index":7,"value":1500}"#, &st).await;
    assert_eq!(resp["ok"], true);
    let (values, _) = st.merge.lock().await.current(Instant::now()).unwrap();
    assert_eq!(values[7], 1500);
}

/// The (pilot, m1) transport flags of [`two_module_state`].
type Flags = (Arc<AtomicBool>, Arc<AtomicBool>);

/// A lane with a pilot `m0` and one autonomy module `m1`, both connected.
/// Returns the autonomy merge and the two connection flags.
fn two_module_state() -> (CmdState, Arc<Mutex<SourceMerge>>, Flags) {
    let autonomy = Arc::new(Mutex::new(SourceMerge::new(ChannelSourceMode::Inject)));
    let pilot_up = Arc::new(AtomicBool::new(true));
    let m1_up = Arc::new(AtomicBool::new(true));
    let mut st = state();
    st.modules = Arc::new(ModuleTargets {
        pilot_id: Some("m0".to_string()),
        pilot_connected: pilot_up.clone(),
        autonomy: vec![ModuleTarget {
            id: "m1".to_string(),
            merge: autonomy.clone(),
            connected: m1_up.clone(),
        }],
    });
    (st, autonomy, (pilot_up, m1_up))
}

#[tokio::test]
async fn the_autonomy_module_stays_addressable_while_the_pilot_is_down() {
    let (st, autonomy, (pilot_up, m1_up)) = two_module_state();
    pilot_up.store(false, Ordering::Relaxed);
    let resp = dispatch(
        &serde_json::to_vec(
            &json!({"op":"set_channels","channels":vec![1200u16;16],"module":"m1"}),
        )
        .unwrap(),
        &st,
    )
    .await;
    assert_eq!(resp["ok"], true);
    assert!(autonomy.lock().await.current(Instant::now()).is_some());
    // The pilot lane and its parameter lane refuse while its port is down.
    let resp = dispatch(br#"{"op":"set_channel","index":0,"value":992}"#, &st).await;
    assert_eq!(resp["error"], "E_MODULE_DISCONNECTED: m0");
    let resp = dispatch(br#"{"op":"param_write","field_index":3,"data":[2]}"#, &st).await;
    assert_eq!(resp["error"], "E_MODULE_DISCONNECTED: m0");
    assert!(st.oob.drain().is_empty());
    // A down autonomy module refuses too.
    m1_up.store(false, Ordering::Relaxed);
    let resp = dispatch(
        br#"{"op":"set_channel","index":0,"value":992,"module":"m1"}"#,
        &st,
    )
    .await;
    assert_eq!(resp["error"], "E_MODULE_DISCONNECTED: m1");
}

#[tokio::test]
async fn injection_addressed_to_an_autonomy_module_lands_on_its_merge() {
    let (st, autonomy, _) = two_module_state();
    let resp = dispatch(
        &serde_json::to_vec(
            &json!({"op":"set_channels","channels":vec![1200u16;16],"module":"m1"}),
        )
        .unwrap(),
        &st,
    )
    .await;
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["module"], "m1");
    let (values, _) = autonomy.lock().await.current(Instant::now()).unwrap();
    assert_eq!(values, [1200u16; 16]);
    // The pilot lane is untouched.
    assert!(st.merge.lock().await.current(Instant::now()).is_none());

    let resp = dispatch(
        br#"{"op":"set_channel","index":2,"value":1500,"module":"m1"}"#,
        &st,
    )
    .await;
    assert_eq!(resp["module"], "m1");
    let (values, _) = autonomy.lock().await.current(Instant::now()).unwrap();
    assert_eq!(values[2], 1500);
}

#[tokio::test]
async fn injection_without_a_module_or_naming_the_pilot_lands_on_the_pilot() {
    let (st, autonomy, _) = two_module_state();
    for line in [
        json!({"op":"set_channels","channels":vec![1000u16;16]}),
        json!({"op":"set_channels","channels":vec![1000u16;16],"module":"m0"}),
    ] {
        let resp = dispatch(&serde_json::to_vec(&line).unwrap(), &st).await;
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["module"], "m0");
    }
    assert!(st.merge.lock().await.current(Instant::now()).is_some());
    assert!(autonomy.lock().await.current(Instant::now()).is_none());
}

#[tokio::test]
async fn injection_naming_an_unknown_module_is_refused() {
    let (st, autonomy, _) = two_module_state();
    let resp = dispatch(
        br#"{"op":"set_channel","index":0,"value":992,"module":"m9"}"#,
        &st,
    )
    .await;
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error"], "E_UNKNOWN_MODULE: m9");
    assert!(st.merge.lock().await.current(Instant::now()).is_none());
    assert!(autonomy.lock().await.current(Instant::now()).is_none());
}

#[tokio::test]
async fn injection_under_hid_authority_reports_it() {
    // In hid mode the injection is stored but the HID path holds
    // authority; the reply must say so, never imply the values fly.
    let st = state_with(ChannelSourceMode::Hid);
    let resp = dispatch(
        &serde_json::to_vec(&json!({"op":"set_channels","channels":vec![1000u16;16]})).unwrap(),
        &st,
    )
    .await;
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["authority"], "hid");
    // Nothing is transmitted (no HID data): the injection did not leak
    // through.
    assert!(st.merge.lock().await.current(Instant::now()).is_none());
}

#[test]
fn parse_validates_param_write() {
    match parse_command(br#"{"op":"param_write"}"#) {
        Parsed::Reply(v) => assert_eq!(v["error"], "E_MISSING_FIELD_INDEX"),
        other => panic!("unexpected {other:?}"),
    }
    let oversize = serde_json::to_vec(
        &json!({"op":"param_write","field_index":1,"data":vec![0u8;MAX_PARAM_DATA+1]}),
    )
    .unwrap();
    match parse_command(&oversize) {
        Parsed::Reply(v) => assert!(v["error"]
            .as_str()
            .unwrap()
            .starts_with("E_PARAM_DATA_TOO_LARGE")),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        parse_command(br#"{"op":"param_write","field_index":3,"data":[2]}"#),
        Parsed::Cmd(Command::ParamWrite {
            field_index: 3,
            data: vec![2],
        })
    );
}

#[tokio::test]
async fn param_write_queues_a_valid_wire_frame() {
    let st = state();
    let resp = dispatch(br#"{"op":"param_write","field_index":3,"data":[2]}"#, &st).await;
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["field_index"], 3);
    assert_eq!(resp["queued"], true);
    let frames = st.oob.drain();
    assert_eq!(frames.len(), 1);
    let (frame, _) = crate::frame::parse_frame(&frames[0]).unwrap();
    assert_eq!(frame.frame_type, crate::frame::TYPE_PARAMETER_WRITE);
    let decoded = ParameterWrite::decode(&frame.payload).unwrap();
    assert_eq!(decoded.dest, ADDR_TRANSMITTER_MODULE);
    assert_eq!(decoded.origin, ADDR_HANDSET);
    assert_eq!(decoded.field_index, 3);
    assert_eq!(decoded.data, vec![2]);
}

#[tokio::test]
async fn param_write_reports_a_full_queue() {
    let st = state();
    for _ in 0..crate::transport::OOB_QUEUE_CAP {
        let resp = dispatch(br#"{"op":"param_write","field_index":1,"data":[0]}"#, &st).await;
        assert_eq!(resp["ok"], true);
    }
    let resp = dispatch(br#"{"op":"param_write","field_index":1,"data":[0]}"#, &st).await;
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error"], "E_QUEUE_FULL");
}

#[tokio::test]
async fn status_merges_the_latest_sidecar_body() {
    let st = state();
    // Before any heartbeat: honest null status.
    let resp = dispatch(br#"{"op":"status"}"#, &st).await;
    assert_eq!(resp["ok"], true);
    assert!(resp["status"].is_null());
    // After a heartbeat wrote a body: the body rides under ok:true.
    *st.latest_status.lock().await = json!({"state": "ready", "rssi_dbm": null});
    let resp = dispatch(br#"{"op":"status"}"#, &st).await;
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["state"], "ready");
    assert!(resp["rssi_dbm"].is_null());
}

#[tokio::test]
async fn end_to_end_over_a_real_unix_socket() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("crsf-cmd.sock");
    let st = state();
    let server = tokio::spawn({
        let st = st.clone();
        let sock = sock.clone();
        async move {
            let _ = serve(st, &sock).await;
        }
    });
    // Wait for the socket to appear.
    for _ in 0..100 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let mut stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
    stream
        .write_all(b"{\"op\":\"set_channel\",\"index\":2,\"value\":992}\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let resp: Value = serde_json::from_slice(&buf).unwrap();
    assert_eq!(resp["ok"], true);
    let (values, _) = st.merge.lock().await.current(Instant::now()).unwrap();
    assert_eq!(values[2], 992);
    server.abort();
}

/// A paired node: the command socket is group-reachable and the current
/// pilot-in-command holder's name is readable from the claim routes, so
/// echoing that name back used to hand the transmit lane to the caller.
#[tokio::test]
async fn naming_the_holder_no_longer_takes_the_lane_from_them() {
    use crate::sources::PicView;

    let dir = tempfile::tempdir().unwrap();
    let pairing = dir.path().join("pairing.json");
    std::fs::write(&pairing, r#"{"paired": true, "api_key": "node-key"}"#).unwrap();

    let st = CmdState {
        merge: Arc::new(Mutex::new(SourceMerge::new(ChannelSourceMode::Hybrid))),
        latest_status: Arc::new(Mutex::new(Value::Null)),
        oob: Arc::new(OobQueue::default()),
        injector_auth: Arc::new(InjectorAuth::new(pairing.clone())),
        modules: Arc::new(single_pilot()),
    };
    // The human holds the claim, and the arbiter is reporting it.
    st.merge.lock().await.set_pic(Some(PicView {
        claimed: true,
        holder: Some("operator-a".to_string()),
    }));

    // An injector that simply says it is the holder.
    let resp = dispatch(
        &serde_json::to_vec(&json!({
            "op": "set_channels",
            "channels": vec![1811u16; 16],
            "client_id": "operator-a",
        }))
        .unwrap(),
        &st,
    )
    .await;
    assert_eq!(resp["ok"], true, "the injection is still accepted");
    assert_eq!(
        resp["authority"], "hid",
        "an unattested claim on the holder's name must not win the lane"
    );

    // The same injector, now carrying a ticket the node minted for it.
    let ticket = crate::injector::mint_injector_ticket(&pairing, "operator-a", 60).unwrap();
    let resp = dispatch(
        &serde_json::to_vec(&json!({
            "op": "set_channels",
            "channels": vec![1811u16; 16],
            "client_id": "operator-a",
            "client_ticket": ticket,
        }))
        .unwrap(),
        &st,
    )
    .await;
    assert_eq!(
        resp["authority"], "inject",
        "an attested holder is the one case the programmatic lane wins"
    );
}
