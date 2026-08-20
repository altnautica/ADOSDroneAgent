//! Auxiliary-stream application delivery: Rust plugin <-> Rust host over real
//! Contract C.
//!
//! Stands up a live `ados-plugin-host` server with a host that arms a
//! `radio.aux_stream.subscribe_stream` from a broadcast channel, connects this
//! SDK's client to it over a Unix socket, subscribes for application datagrams,
//! and asserts a `(channel, payload)` the host pushes reaches the plugin-side
//! aux callback. This exercises the full path: server `radio.aux_stream.subscribe`
//! arm -> `radio.aux_stream.deliver` push -> SDK reader loop `radio.aux_stream.
//! deliver` branch -> the registered aux callback. The channel and payload are
//! byte-checked against what the host pushed so wire parity with the radio lane
//! holds.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ados_plugin_host::host::HostServices;
use ados_plugin_host::{EventBus, PluginIpcServer};
use ados_protocol::plugin::TokenIssuer;
use ados_sdk::PluginIpcClient;
use rmpv::Value;
use tokio::sync::broadcast;

const PLUGIN_ID: &str = "com.example.aux";

fn caps(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// A host that arms an aux application-datagram stream from a broadcast channel
/// the test pushes `(channel, payload)` into. Every other host method stays at
/// the trait default, which is all the aux-delivery path needs.
struct AuxStreamHost {
    app: broadcast::Sender<(u8, Vec<u8>)>,
}

impl HostServices for AuxStreamHost {
    fn radio_aux_stream_subscribe_stream(
        &self,
        _plugin_id: &str,
    ) -> Option<broadcast::Receiver<(u8, Vec<u8>)>> {
        Some(self.app.subscribe())
    }
}

struct Harness {
    issuer: Arc<TokenIssuer>,
    path: std::path::PathBuf,
    app: broadcast::Sender<(u8, Vec<u8>)>,
    _accept: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let issuer = Arc::new(TokenIssuer::new(b"aux-delivery-secret".to_vec()));
    let bus = Arc::new(EventBus::new());
    let (app, _rx) = broadcast::channel(64);
    let host = Arc::new(AuxStreamHost {
        app: app.clone(),
    });
    let server = PluginIpcServer::new(dir.path(), issuer.clone(), bus, host);
    let (path, accept) = server.serve_plugin(PLUGIN_ID).expect("bind plugin socket");
    Harness {
        issuer,
        path,
        app,
        _accept: accept,
        _dir: dir,
    }
}

async fn connect(h: &Harness, granted: &[&str]) -> Arc<PluginIpcClient> {
    let token = h
        .issuer
        .mint(PLUGIN_ID, &caps(granted), 600)
        .to_token_string();
    let ipc = Arc::new(PluginIpcClient::new(PLUGIN_ID, token, &h.path));
    ipc.connect().await.expect("connect + handshake");
    ipc
}

#[tokio::test]
async fn pushed_aux_datagram_reaches_the_aux_callback() {
    let h = harness();
    let ipc = connect(&h, &["radio.aux_stream"]).await;

    let hits = Arc::new(AtomicUsize::new(0));
    let last: Arc<Mutex<Option<(u8, Vec<u8>)>>> = Arc::new(Mutex::new(None));
    let h_hits = hits.clone();
    let h_last = last.clone();
    ipc.register_aux_callback(Arc::new(move |args: Value| {
        h_hits.fetch_add(1, Ordering::Relaxed);
        if let Value::Map(m) = &args {
            let ch = m
                .iter()
                .find(|(k, _)| k.as_str() == Some("channel"))
                .and_then(|(_, v)| v.as_u64())
                .unwrap_or(0) as u8;
            let pl = match m
                .iter()
                .find(|(k, _)| k.as_str() == Some("payload"))
                .map(|(_, v)| v)
            {
                Some(Value::Binary(b)) => b.clone(),
                _ => Vec::new(),
            };
            *h_last.lock().unwrap() = Some((ch, pl));
        }
    }));

    // Arm the aux subscribe so the server spawns its forwarder.
    ipc.radio_aux_stream_subscribe()
        .await
        .expect("radio_aux_stream.subscribe");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Push one application datagram on the AppStream channel (8).
    let want: (u8, Vec<u8>) = (8, vec![0xde, 0xad, 0xbe, 0xef]);
    h.app.send(want.clone()).expect("push datagram");

    let mut saw = false;
    for _ in 0..100 {
        if hits.load(Ordering::Relaxed) > 0 {
            saw = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(saw, "the radio.aux_stream.deliver push never reached the callback");
    assert_eq!(
        last.lock().unwrap().as_ref(),
        Some(&want),
        "the delivered (channel, payload) must match what the host pushed"
    );

    ipc.close().await;
}
