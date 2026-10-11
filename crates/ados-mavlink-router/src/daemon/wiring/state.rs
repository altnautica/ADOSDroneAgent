//! The onboard swarm loop and the 10 Hz state publish.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_protocol::ipc::IpcBroadcast;
use ados_protocol::shutdown::Shutdown;
use ados_protocol::state::encode_v2;
use tokio::sync::Mutex;

use super::super::extras;
use super::super::startup::ProxyPosture;
use super::super::Tasks;
use super::aux_link::AuxWiring;
use crate::connection::swarm_setpoint::{self, SwarmSetpointStatus};
use crate::connection::FcConnection;
use crate::param_cache::ParamCache;
use crate::state::VehicleState;

/// Spawn the swarm setpoint loop and the state publish that reports it.
#[allow(clippy::too_many_arguments)]
pub(in crate::daemon) fn spawn(
    fc: &Arc<FcConnection>,
    state: &Arc<Mutex<VehicleState>>,
    params: &Arc<Mutex<ParamCache>>,
    dir: &str,
    mavlink_ipc: &Arc<IpcBroadcast>,
    state_ipc: &Arc<IpcBroadcast>,
    started: Instant,
    aux: &AuxWiring,
    posture: &ProxyPosture,
    cancel: &Shutdown,
    tasks: &mut Tasks,
) {
    // 10 Hz state publish: vehicle snapshot + the service runtime extras.
    // Onboard swarm autonomy. The loop reads the swarm neighbour table off
    // `/run/ados/swarm.sock`, runs the control laws at 10 Hz and commands the FC
    // through this router's own send path. `SwarmSetpointStatus` is the reverse
    // direction: the active precedence level and the emergency condition, which the
    // state snapshot below republishes for `ados-swarmbus` to fold into the beacon.
    // While the swarm is disabled or no drone slot is assigned the loop holds no
    // socket: it re-checks the config file every 2 s and starts (or stands down)
    // when the `swarm:` block changes, so a settings write needs no restart.
    let swarm_status: Option<Arc<SwarmSetpointStatus>> = {
        let status = Arc::new(SwarmSetpointStatus::default());
        let fc = fc.clone();
        let state = state.clone();
        let swarm_sock = format!("{dir}/swarm.sock");
        let handle = status.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            swarm_setpoint::run(
                fc,
                state,
                swarm_sock,
                crate::config::CONFIG_YAML.to_string(),
                handle,
                cancel,
            )
            .await
        }));
        Some(status)
    };

    {
        let fc = fc.clone();
        let state = state.clone();
        let params = params.clone();
        let state_ipc = state_ipc.clone();
        let swarm_status = swarm_status.clone();
        let mavlink_ipc_stats = mavlink_ipc.clone();
        let aux_tee_counters = aux.aux_tee_counters.clone();
        let aux_rpc_counters = aux.aux_rpc_counters.clone();
        let frame_ingest_counters = aux.frame_ingest_counters.clone();
        let relayed_vehicle = aux.relayed_vehicle.clone();
        let uplink_sender = aux.uplink_sender.clone();
        let proxy_posture = posture.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(100));
            // Last reported eviction counts, so a fresh eviction logs once
            // rather than every 100 ms tick while the count sits unchanged.
            let mut last_mavlink_drops = 0u64;
            let mut last_state_drops = 0u64;
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        let mavlink_drops = mavlink_ipc_stats.dropped_clients();
                        let state_drops = state_ipc.dropped_clients();
                        // Surface a newly evicted slow consumer as a log line
                        // (also shipped to the logging daemon) so the eviction
                        // is not silent. The continuous signal rides the state
                        // snapshot below for the GCS.
                        if mavlink_drops > last_mavlink_drops {
                            tracing::warn!(
                                socket = "mavlink",
                                evicted = mavlink_drops - last_mavlink_drops,
                                total = mavlink_drops,
                                "ipc_slow_client_evicted"
                            );
                            last_mavlink_drops = mavlink_drops;
                        }
                        if state_drops > last_state_drops {
                            tracing::warn!(
                                socket = "state",
                                evicted = state_drops - last_state_drops,
                                total = state_drops,
                                "ipc_slow_client_evicted"
                            );
                            last_state_drops = state_drops;
                        }
                        let extras = extras::build_extras(
                            &fc, &state, &params, started, mavlink_drops, state_drops,
                            aux_tee_counters.as_ref(), frame_ingest_counters.as_ref(),
                            aux_rpc_counters.as_ref(), swarm_status.as_ref(),
                            uplink_sender.as_ref(),
                            relayed_vehicle.as_ref(),
                            &proxy_posture,
                        )
                        .await;
                        let wire = { state.lock().await.to_wire_with(&extras) };
                        let encoded = encode_v2(&wire);
                        match encoded {
                            Ok(bytes) => state_ipc.broadcast(bytes.into()).await,
                            Err(e) => tracing::warn!(error = %e, "state_encode_failed"),
                        }
                    }
                    _ = cancel.wait() => break,
                }
            }
        }));
    }
}
