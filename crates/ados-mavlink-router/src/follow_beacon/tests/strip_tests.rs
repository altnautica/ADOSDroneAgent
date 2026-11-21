//! `FOLLOW_TARGET` from a ground station reaches the plugin bus only: never
//! the other clients (the MAVLink socket, the cloud relay), the flight
//! controller or the aux uplink, unless the FC is opted in.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::AsyncWrite;
use tokio::sync::{broadcast, Mutex};

use super::{fix, frame};
use crate::config::MavlinkConfig;
use crate::connection::{ClientOrigin, FcConnection};
use crate::follow_beacon::strip;
use crate::param_cache::ParamCache;
use crate::routing::RouteHub;
use crate::rx_frame::RxFrame;
use crate::state::VehicleState;
use ados_protocol::mavlink::ardupilotmega::HEARTBEAT_DATA;
use ados_protocol::mavlink::MavMessage;

fn heartbeat() -> Vec<u8> {
    frame(&MavMessage::HEARTBEAT(HEARTBEAT_DATA::DEFAULT))
}

fn beacon() -> Vec<u8> {
    frame(&MavMessage::FOLLOW_TARGET(fix(5_000, 3.0)))
}

#[test]
fn strip_takes_out_only_follow_target() {
    let hb = heartbeat();
    let partial = &hb[..5];
    let mut data = hb.clone();
    data.extend(beacon());
    data.extend(&hb);
    data.extend(partial);
    let mut want = hb.clone();
    want.extend(&hb);
    want.extend(partial);
    assert_eq!(&*strip(&data), &want[..]);
    assert!(strip(&beacon()).is_empty());
    assert!(matches!(strip(&hb), std::borrow::Cow::Borrowed(_)));
}

#[tokio::test]
async fn route_aware_forwarding_never_hands_it_to_another_client() {
    let hub = RouteHub::new(true);
    let (tx, _) = broadcast::channel::<RxFrame>(8);
    let (_a, ep_a) = hub.client_feed("ws", tx.subscribe());
    let (mut ipc, _) = hub.client_feed("ipc", tx.subscribe());
    let ep_a = ep_a.unwrap();
    hub.ingress(&ep_a, &beacon());
    let mut mixed = heartbeat();
    mixed.extend(beacon());
    hub.ingress(&ep_a, &mixed);
    let got = tokio::time::timeout(Duration::from_millis(100), ipc.recv())
        .await
        .expect("the heartbeat is routed")
        .unwrap();
    assert_eq!(&got.bytes[..], &heartbeat()[..]);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), ipc.recv())
            .await
            .is_err(),
        "nothing else"
    );
}

struct CapturingWriter(Arc<std::sync::Mutex<Vec<u8>>>);

impl AsyncWrite for CapturingWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.0.lock().unwrap().extend_from_slice(data);
        Poll::Ready(Ok(data.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn connection(cfg: MavlinkConfig) -> Arc<FcConnection> {
    let state = Arc::new(Mutex::new(VehicleState::default()));
    let params = Arc::new(Mutex::new(ParamCache::new(
        "/tmp/ados-beacon-strip-params.json",
    )));
    FcConnection::new(cfg, state, params)
}

/// What the FC writer and the client-write tap saw of one write `data`.
async fn fc_sees(cfg: MavlinkConfig, data: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let fc = connection(cfg);
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    fc.connected
        .store(true, std::sync::atomic::Ordering::Relaxed);
    *fc.writer.lock().await = Some(Box::pin(CapturingWriter(captured.clone())));
    let mut tap = fc.subscribe_client_writes();
    assert!(
        fc.send_client_bytes(data, ClientOrigin::Trusted, None)
            .await
    );
    let tapped = tap.try_recv().map(|b| b.to_vec()).unwrap_or_default();
    let written = captured.lock().unwrap().clone();
    (written, tapped)
}

#[tokio::test]
async fn the_flight_controller_never_gets_it_unless_opted_in() {
    let mut mixed = heartbeat();
    mixed.extend(beacon());

    let (written, tapped) = fc_sees(MavlinkConfig::default(), &mixed).await;
    assert_eq!(written, heartbeat());
    assert_eq!(tapped, heartbeat());
    let (written, tapped) = fc_sees(MavlinkConfig::default(), &beacon()).await;
    assert!(written.is_empty() && tapped.is_empty());

    let opted_in = MavlinkConfig {
        follow_target_to_fc: true,
        ..MavlinkConfig::default()
    };
    let (written, _) = fc_sees(opted_in, &mixed).await;
    assert_eq!(written, mixed);
}

#[tokio::test]
async fn the_aux_uplink_never_carries_it() {
    let fc = connection(MavlinkConfig::default());
    let listener = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    fc.set_aux_uplink(crate::aux_uplink::spawn(port)).await;

    assert!(
        fc.send_client_bytes(&beacon(), ClientOrigin::Trusted, None)
            .await
    );
    let mut mixed = beacon();
    mixed.extend(heartbeat());
    assert!(
        fc.send_client_bytes(&mixed, ClientOrigin::Trusted, None)
            .await
    );

    let mut buf = [0u8; 512];
    let (n, _) = tokio::time::timeout(Duration::from_millis(300), listener.recv_from(&mut buf))
        .await
        .expect("the heartbeat crosses")
        .unwrap();
    let (_, payload) = ados_protocol::aux_mux::decode(&buf[..n]).unwrap();
    assert_eq!(payload, heartbeat());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.recv_from(&mut buf))
            .await
            .is_err(),
        "nothing else crosses"
    );
}
