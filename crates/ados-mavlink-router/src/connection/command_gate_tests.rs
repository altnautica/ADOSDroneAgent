use super::*;
use crate::param_cache::ParamCache;
use crate::state::VehicleState;
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::task::{Context, Poll};
use tokio::io::AsyncWrite;

fn conn_with(cfg: MavlinkConfig) -> std::sync::Arc<FcConnection> {
    let state = std::sync::Arc::new(Mutex::new(VehicleState::default()));
    let params = std::sync::Arc::new(Mutex::new(ParamCache::new(
        "/tmp/ados-crsf-cmd-gate-params.json",
    )));
    FcConnection::new(cfg, state, params)
}

/// A write half that counts the bytes handed to it, standing in for the FC
/// carrier so a test can prove whether a send path reached the FC.
struct CountingWriter(std::sync::Arc<AtomicUsize>);
impl AsyncWrite for CountingWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.0.fetch_add(data.len(), Ordering::Relaxed);
        Poll::Ready(Ok(data.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// The gated default: the lane opted in, MAVLink mode, a pinned serial
/// carrier, and NO command marker — a live MAVLink-over-ELRS source that is
/// telemetry-only.
fn gated_mavlink_cfg() -> MavlinkConfig {
    MavlinkConfig {
        crsf_enabled: true,
        crsf_mode: "mavlink".into(),
        crsf_device: "/dev/ttyUSB0".into(),
        ..MavlinkConfig::default()
    }
}

#[test]
fn a_crsf_mavlink_source_defaults_to_command_down_gated() {
    let c = conn_with(gated_mavlink_cfg());
    assert!(c.cfg.crsf_mavlink_source().is_some(), "the source resolves");
    assert!(
        c.command_down_gated(),
        "marker off + crsf mavlink source → command-down gated"
    );
}

#[test]
fn the_command_marker_restores_the_writer_path() {
    // Flipping the marker on is the bench-gate switch: the same source is
    // no longer gated, so run() installs a writer for the command lane.
    let mut cfg = gated_mavlink_cfg();
    cfg.crsf_mavlink_command_enabled = true;
    let c = conn_with(cfg);
    assert!(c.cfg.crsf_mavlink_source().is_some());
    assert!(
        !c.command_down_gated(),
        "marker on → writer restored (not gated)"
    );
}

#[test]
fn other_fc_sources_are_never_command_down_gated() {
    // A plain serial FC is not a CRSF source, so it is never gated — its
    // command path stays open regardless of the marker's value.
    for marker in [false, true] {
        let serial = conn_with(MavlinkConfig {
            serial_port: "/dev/ttyACM0".into(),
            crsf_mavlink_command_enabled: marker,
            ..MavlinkConfig::default()
        });
        assert_eq!(serial.cfg.crsf_mavlink_source(), None);
        assert!(!serial.command_down_gated(), "marker={marker}");
    }
    // A UDP SITL transport: same — not a CRSF source, never gated.
    let udp = conn_with(MavlinkConfig {
        source: "udp".into(),
        serial_port: "udp:127.0.0.1:14550".into(),
        ..MavlinkConfig::default()
    });
    assert!(!udp.command_down_gated());
}

#[tokio::test]
async fn a_gated_source_installs_no_writer_and_suppresses_every_send_path() {
    // Mirror run()'s gated branch: a gated source installs no writer. With
    // the transport open (so the scheduler bodies run) the companion
    // heartbeat, the stream requests, and a forwarded client command all
    // reach a NULL writer — nothing is transmitted toward the FC, and no
    // reconnect is raised (there was no writer to fail). Inbound telemetry
    // is unaffected: the read loop never touches the writer.
    let c = conn_with(gated_mavlink_cfg());
    assert!(c.command_down_gated());
    c.connected.store(true, Ordering::Relaxed);
    assert!(
        c.writer.lock().await.is_none(),
        "the gated branch installs no writer"
    );
    c.send_heartbeat().await;
    c.tick_streams().await;
    c.send_bytes(b"\xfd\x00\x00\x00").await;
    assert!(
        c.writer.lock().await.is_none(),
        "no send path creates a writer for a gated source"
    );
    let signalled = tokio::time::timeout(
        Duration::from_millis(50),
        c.reconnect_requested(c.link_gen()),
    )
    .await
    .is_ok();
    assert!(
        !signalled,
        "a gated source with no writer raises no reconnect"
    );
}

#[tokio::test]
async fn an_ungated_source_writes_the_companion_heartbeat_to_the_fc() {
    // The other side of the gate: a non-CRSF FC source installs a writer,
    // so the send scheduler reaches the FC. Count the heartbeat bytes that
    // land on the writer to prove the command path still works — the WFB /
    // serial FC path must be unaffected by the CRSF gate.
    let c = conn_with(MavlinkConfig {
        serial_port: "/dev/ttyACM0".into(),
        ..MavlinkConfig::default()
    });
    assert!(!c.command_down_gated());
    let written = std::sync::Arc::new(AtomicUsize::new(0));
    c.connected.store(true, Ordering::Relaxed);
    *c.writer.lock().await = Some(Box::pin(CountingWriter(written.clone())));
    c.send_heartbeat().await;
    assert!(
        written.load(Ordering::Relaxed) > 0,
        "the FC command path wrote the heartbeat"
    );
}

/// The parameter sweep's send-failure flag must reflect whether the
/// PARAM_REQUEST_LIST reached the FC. With no writer it never left, and the
/// GCS must be told that rather than shown an indefinite "priming".
#[tokio::test]
async fn a_sweep_that_never_reaches_the_fc_reports_the_send_failure() {
    let c = conn_with(gated_mavlink_cfg());
    c.connected.store(true, Ordering::Relaxed);
    c.tick_param_sweep().await;
    assert!(c.param_priming());
    assert!(
        c.param_sweep_send_failed(),
        "a request that reached no writer is a failed send"
    );
}
