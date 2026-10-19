//! The link supervisor: resolve and open the transport, run sessions with
//! reconnect backoff, forward RTCM from the bus, and write the sidecar.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::shutdown::Shutdown;
use ados_sensorbus::time::mono_ns;
use ados_sensorbus::{Kind, Origin, Publisher, Subscriber};
use tokio::sync::{mpsc, Notify};

use crate::config::{HubConfig, TransportSpec};
use crate::rig::Extrinsics;
use crate::session::{lock, run_session, SessionCtx, SessionEnd, Stop};
use crate::state::{write_sidecar, RateMeter, SharedState};
use crate::transport::{open, resolve};

const MIN_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(10);
/// Backoff after the hub reported a protocol version this build cannot read.
const UNSUPPORTED_BACKOFF: Duration = Duration::from_secs(30);
const SIDECAR_INTERVAL: Duration = Duration::from_secs(1);
const BUS_RETRY: Duration = Duration::from_secs(1);
/// RTCM payloads queued toward the hub before new ones are dropped.
const RTCM_QUEUE: usize = 64;

/// Everything one link run needs.
pub struct LinkParams {
    pub cfg: HubConfig,
    /// The `/sys/class/tty`-shaped root `auto` resolves against.
    pub tty_root: PathBuf,
    pub ext: Extrinsics,
    /// The sensor-bus socket.
    pub socket: PathBuf,
    pub sidecar: PathBuf,
}

async fn stop_signal(shutdown: &Shutdown, reload: &Notify) -> Stop {
    tokio::select! {
        biased;
        _ = shutdown.wait() => Stop::Shutdown,
        _ = reload.notified() => Stop::Reload,
    }
}

/// Run the link until shutdown or a reload request.
pub async fn run_link(p: LinkParams, shutdown: Shutdown, reload: Arc<Notify>) -> Stop {
    let state = SharedState::default();
    let publisher = Publisher::spawn(p.socket.clone(), Origin::Hub);
    let (rtcm_tx, mut rtcm_rx) = mpsc::channel(RTCM_QUEUE);
    let forwarder = tokio::spawn(rtcm_from_bus(p.socket.clone(), p.cfg.clone(), rtcm_tx));
    let sidecar = tokio::spawn(sidecar_loop(p.sidecar.clone(), state.clone()));

    let mut backoff = MIN_BACKOFF;
    let why = loop {
        let wait = match resolve(&p.cfg.transport, &p.tty_root) {
            None => {
                tracing::debug!("sensor hub not attached");
                backoff
            }
            Some(ep) => {
                lock(&state).transport = ep.label();
                let opened = tokio::select! {
                    biased;
                    why = stop_signal(&shutdown, &reload) => break why,
                    opened = open(&ep) => opened,
                };
                match opened {
                    Err(e) => {
                        tracing::warn!(transport = %ep.label(), error = %e, "sensor hub open failed");
                        backoff
                    }
                    Ok(io) => {
                        tracing::info!(transport = %ep.label(), "sensor hub connected");
                        lock(&state).begin_session(p.cfg.imu_odr_hz, p.cfg.trigger_hz);
                        let ctx = SessionCtx {
                            cfg: &p.cfg,
                            ext: &p.ext,
                            publisher: &publisher,
                            rtcm: &mut rtcm_rx,
                            state: &state,
                        };
                        let (end, packets) =
                            run_session(io, ctx, stop_signal(&shutdown, &reload)).await;
                        lock(&state).end_session();
                        if packets > 0 {
                            backoff = MIN_BACKOFF;
                        }
                        match end {
                            SessionEnd::Stopped(why) => break why,
                            SessionEnd::Closed => {
                                tracing::warn!(packets, "sensor hub closed the link");
                                backoff
                            }
                            SessionEnd::Io(e) => {
                                tracing::warn!(packets, error = %e, "sensor hub link error");
                                backoff
                            }
                            SessionEnd::Unsupported(v) => {
                                tracing::error!(
                                    proto_ver = v,
                                    "sensor hub protocol version unsupported; not streaming"
                                );
                                UNSUPPORTED_BACKOFF
                            }
                        }
                    }
                }
            }
        };
        tokio::select! {
            biased;
            why = stop_signal(&shutdown, &reload) => break why,
            _ = tokio::time::sleep(wait) => {}
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    };

    forwarder.abort();
    sidecar.abort();
    let _ = sidecar.await;
    // A last write so a stopping service does not leave `connected: true`.
    let body = RateMeter::default().body(&lock(&state), mono_ns());
    let path = p.sidecar.clone();
    let _ = tokio::task::spawn_blocking(move || write_sidecar(&path, &body)).await;
    why
}

/// Subscribe to `rtcm` frames and queue the payloads of the configured origins
/// toward the hub, reconnecting to the bus as needed. A full queue drops the
/// newest payload.
async fn rtcm_from_bus(socket: PathBuf, cfg: HubConfig, tx: mpsc::Sender<Vec<u8>>) {
    loop {
        if let Ok(mut sub) = Subscriber::connect(&socket, &[Kind::Rtcm]).await {
            loop {
                match sub.recv().await {
                    Ok(f) if f.kind == Kind::Rtcm && cfg.forwards_rtcm_from(f.origin) => {
                        let _ = tx.try_send(f.payload);
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {}
                    Err(_) => break,
                }
            }
        }
        tokio::time::sleep(BUS_RETRY).await;
    }
}

async fn sidecar_loop(path: PathBuf, state: SharedState) {
    let mut meter = RateMeter::default();
    let mut tick = tokio::time::interval(SIDECAR_INTERVAL);
    let mut reported = false;
    loop {
        tick.tick().await;
        let body = {
            let st = lock(&state);
            meter.body(&st, mono_ns())
        };
        let target = path.clone();
        let written = tokio::task::spawn_blocking(move || write_sidecar(&target, &body))
            .await
            .unwrap_or_else(|e| Err(std::io::Error::other(e)));
        match written {
            Ok(()) => reported = false,
            Err(e) if !reported => {
                tracing::warn!(path = %path.display(), error = %e, "sensorhub sidecar write failed");
                reported = true;
            }
            Err(_) => {}
        }
    }
}

/// Whether the service has anything to do: an explicit transport, or an
/// attached hub for `auto`.
pub fn has_target(spec: &TransportSpec, tty_root: &std::path::Path) -> bool {
    resolve(spec, tty_root).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stop_is_honoured_while_the_transport_cannot_be_reached() {
        let dir = tempfile::tempdir().unwrap();
        // TEST-NET-1: never answers, so the connect hangs or fails outright.
        let params = LinkParams {
            cfg: HubConfig {
                transport: TransportSpec::Tcp("192.0.2.1:7600".into()),
                ..HubConfig::default()
            },
            tty_root: dir.path().join("tty"),
            ext: Extrinsics::default(),
            socket: dir.path().join("sensors.sock"),
            sidecar: dir.path().join("sensorhub.json"),
        };
        let shutdown = Shutdown::new();
        let link = tokio::spawn(run_link(params, shutdown.clone(), Arc::new(Notify::new())));
        tokio::time::sleep(Duration::from_millis(300)).await;
        shutdown.trigger();
        let why = tokio::time::timeout(Duration::from_secs(1), link)
            .await
            .expect("the link stops within 1 s")
            .unwrap();
        assert_eq!(why, Stop::Shutdown);
        let body: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("sensorhub.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(body["connected"], false);
        assert_eq!(body["transport"], "tcp:192.0.2.1:7600");
    }
}
