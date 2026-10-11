//! The radio auxiliary lane: the drone-side tee and uplink consumer, and the
//! ground-station frame ingest and uplink sender.

use std::sync::{Arc, Mutex as StdMutex};

use ados_protocol::ipc::IpcBroadcast;
use ados_protocol::shutdown::Shutdown;

use super::super::startup::aux_ports;
use super::super::Tasks;
use crate::aux_rpc_dedupe::RequestDedupe;
use crate::aux_tee::{self, TeeCounters};
use crate::aux_uplink;
use crate::aux_uplink_consumer::{self, AuxUplinkConsumerCounters};
use crate::config::MavlinkConfig;
use crate::connection::FcConnection;
use crate::frame_ingest::{self, IngestCounters, INGEST_QUEUE_DEPTH};
use crate::relayed::RelayedVehicle;

/// The aux-lane handles the state snapshot reports. Each is `None` on a
/// profile that does not run that lane.
pub(in crate::daemon) struct AuxWiring {
    // Set when the aux MAVLink tee runs, so its counters ride the state
    // snapshot. Absent on a profile that does not run the tee, which reads as
    // "not running" rather than as a lane that forwarded nothing.
    pub(in crate::daemon) aux_tee_counters: Option<Arc<TeeCounters>>,
    // Set when the republish seam runs, on the same "absent means not running"
    // reading as the tee counters above.
    pub(in crate::daemon) frame_ingest_counters: Option<Arc<IngestCounters>>,
    // The decoded reading of the vehicle on the far end of the radio, set on
    // the same profiles as the seam that feeds it. Kept strictly apart from the
    // attached-FC `state` above: this node still has no flight controller of
    // its own, it can now simply see the one it is relaying.
    pub(in crate::daemon) relayed_vehicle: Option<Arc<StdMutex<RelayedVehicle>>>,
    // Set when the relay-proxy uplink runs, on the same reading as the two
    // above: absent means the lane is not running on this profile, which is not
    // the same signal as a lane that received nothing.
    pub(in crate::daemon) aux_rpc_counters: Option<AuxUplinkConsumerCounters>,
    /// The ground station's uplink sender, kept so its held-frame counter rides
    /// the state snapshot. `None` on a drone.
    pub(in crate::daemon) uplink_sender: Option<aux_uplink::AuxUplinkSender>,
}

/// Spawn the aux-lane tasks this profile runs.
pub(in crate::daemon) async fn spawn(
    cfg: &MavlinkConfig,
    fc: &Arc<FcConnection>,
    cancel: &Shutdown,
    dir: &str,
    tasks: &mut Tasks,
) -> AuxWiring {
    let mut aux_tee_counters = None;
    let mut frame_ingest_counters = None;
    let mut relayed_vehicle = None;
    let mut aux_rpc_counters = None;
    // FC frames -> the radio's auxiliary lane, so a ground station linked over
    // the radio gets the vehicle's MAVLink without a shared network. Drone
    // profile only: a ground station is the receiving end of this lane, and its
    // own radio service does not serve the aux transmit pair at all.
    if cfg.is_drone() {
        let counters = Arc::new(TeeCounters::default());
        aux_tee_counters = Some(counters.clone());
        // Another consumer of the same fan-out the IPC socket reads. A slow aux
        // lane makes this receiver lag and shed, never the producer stall.
        let frames = fc.frame_feed(&["aux"]);
        let egress = ados_protocol::aux_egress::AuxEgress::new(format!("{dir}/radio-aux.sock"));
        let tee_cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            aux_tee::run(
                frames,
                egress,
                counters,
                aux_tee::ShaperConfig::default(),
                tee_cancel,
            )
            .await
        }));

        // The other half of the same pair: a ground station's outbound
        // MAVLink (a client's arm/mode/param/mission command, relayed over
        // the radio) arrives decoded on this loopback — the drone's own
        // `wfb_rx -p3` has always run and re-emitted here, but until now
        // nothing read the port, so it landed in a void. Injecting into the
        // FC via `send_bytes` is what actually closes the ground-to-drone
        // half of the relay.
        let uplink_counters = AuxUplinkConsumerCounters::new();
        aux_rpc_counters = Some(uplink_counters.clone());
        // One dedupe cache for the whole process. The ground retransmits a
        // Request datagram it has no answer for — the only way to survive an
        // uplink that loses a fifth of its packets to the video TX burst — so
        // without this a retried PUT would execute twice. Shared by every
        // spawned handler, hence the Arc.
        let uplink_dedupe = Arc::new(RequestDedupe::new());
        let uplink_fc = fc.clone();
        let uplink_cancel = cancel.clone();
        // The drone's own AuxEgress for sending RPC responses back over the
        // aux downlink (radio_id 2). Same command socket the aux_tee uses.
        let uplink_egress = Arc::new(ados_protocol::aux_egress::AuxEgress::new(
            std::path::Path::new(&format!("{dir}/radio-aux.sock")),
        ));
        // Empty only on an unprovisioned box, where every request is accepted:
        // dropping all of them would be the worse failure for a lone drone.
        let uplink_device_id: Arc<str> = Arc::from(cfg.device_id.as_str());
        if uplink_device_id.is_empty() {
            tracing::warn!(
                path = %ados_protocol::identity::device_id_path().display(),
                "aux_rpc_own_device_id_unresolved_accepting_every_target"
            );
        } else {
            tracing::info!(device_id = %uplink_device_id, "aux_rpc_own_device_id_resolved");
        }
        tasks.push(tokio::spawn(async move {
            aux_uplink_consumer::run(
                aux_ports().rx,
                aux_uplink_consumer::UplinkDeps {
                    fc: uplink_fc,
                    egress: Some(uplink_egress),
                    own_device_id: uplink_device_id,
                    dedupe: uplink_dedupe,
                    // The hand-off to the local config-over-radio service.
                    // Harmless when that channel is opted out: nothing is bound
                    // to the ingress port, so a frame is counted and dropped.
                    config_tunnel: Some(Arc::new(
                        ados_protocol::config_tunnel_ingest::ConfigTunnelIngest::at_configured_port(
                        ),
                    )),
                },
                uplink_counters,
                uplink_cancel,
            )
            .await
        }));
    } else {
        tracing::info!(
            profile = %cfg.profile,
            "mavlink_aux_tee_skipped_for_profile"
        );
    }

    // The mirror of the tee, on the receiving rig. A ground station has no
    // flight controller of its own; the vehicle's frames arrive over the radio,
    // are decoded by the ground data plane, and enter here. Publishing them to
    // the fan-out is what makes the transports this router already serves carry
    // the vehicle, so a ground control station connected to the ground station
    // sees it over the ports it already uses. Ground-station profile only: a
    // drone has its own flight controller and must never take frames from
    // off-board as if they were its own.
    let mut uplink_sender: Option<aux_uplink::AuxUplinkSender> = None;
    if cfg.is_ground_station() {
        let ingest_sock = format!(
            "{dir}/{}",
            ados_protocol::mavlink_ingest::MAVLINK_INGEST_SOCK_NAME
        );
        // Bound only as an inbound frame reader. The broadcast direction is
        // deliberately unused: this socket exists to carry frames INTO the
        // fan-out, and a consumer wanting frames back out has the MAVLink
        // socket and the transports already.
        match IpcBroadcast::bind(
            &ingest_sock,
            INGEST_QUEUE_DEPTH,
            false,
            Some(INGEST_QUEUE_DEPTH),
        )
        .await
        {
            Ok((server, inbound)) => {
                let inbound = inbound.expect("inbound channel requested");
                let counters = Arc::new(IngestCounters::default());
                frame_ingest_counters = Some(counters.clone());
                let relayed = Arc::new(StdMutex::new(RelayedVehicle::default()));
                relayed_vehicle = Some(relayed.clone());
                // Held for the process lifetime: dropping the server closes the
                // socket and the ground data plane would find nothing to
                // connect to.
                let server = Arc::new(server);
                let fc = fc.clone();
                let cancel = cancel.clone();
                tasks.push(tokio::spawn(async move {
                    let _server = server;
                    frame_ingest::run(inbound, fc, counters, relayed, cancel).await
                }));
            }
            Err(e) => {
                // Not fatal: without this seam the ground station still serves
                // its own surfaces, it just cannot relay a vehicle. Say so
                // loudly rather than starting up looking healthy.
                tracing::error!(
                    path = %ingest_sock,
                    error = %e,
                    "mavlink_frame_ingest_bind_failed"
                );
            }
        }

        // The other half of the same relay: a connected client's outbound
        // MAVLink (arm, mode, param read/write, mission commands, ...) used
        // to reach this ground station and go no further — `send_bytes`
        // wrote to a local FC that does not exist, and silently dropped
        // everything. This installs the fallback `send_client_bytes` uses:
        // frame + batch + radiate on the aux uplink (radio_id 3) toward
        // whichever drone the ground station's WFB link has bound, closing
        // the ground-to-drone half of the relay (the drone-to-ground half
        // has run since the aux downlink lane was wired up).
        let sender = aux_uplink::spawn(aux_ports().tx);
        uplink_sender = Some(sender.clone());
        fc.set_aux_uplink(sender).await;
    }
    AuxWiring {
        aux_tee_counters,
        frame_ingest_counters,
        relayed_vehicle,
        aux_rpc_counters,
        uplink_sender,
    }
}
