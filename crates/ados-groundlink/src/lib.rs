//! Ground-station data-plane: WFB receive, channel acquisition, video fan-out,
//! and the self-healing mesh. Modules are added incrementally; this is the
//! crate root.
//!
//! The radio adapter lifecycle and the FHSS/TX-liveness machinery live in the
//! sibling `ados-radio` crate; this crate owns the receive-side glue: the video
//! UDP fan-out, the Contract-E sidecar files, the channel-acquisition receive
//! loop + presence cache, the batman-adv mesh + relay/receiver FEC supervision,
//! and the mesh tap-to-pair crypto.

pub mod acquire;
pub mod fanout;
pub mod gs_config;
pub mod mdns;
pub mod mesh;
pub mod mesh_events;
pub mod pairing;
pub mod paths;
pub mod presence;
pub mod process_spawn;
pub mod receiver;
pub mod relay;
pub mod sidecars;
pub mod watchdog;
pub mod wfb_rx;

pub use acquire::{AcquireState, ChannelAcquirer};
pub use fanout::{run_default_fanout, run_fanout};
pub use gs_config::GroundStationConfig;
pub use mesh::{get_current_role, MeshSnapshot};
pub use pairing::{decrypt_invite, encrypt_invite, InviteBundle};
pub use presence::GsPresenceCache;
pub use receiver::ReceiverState;
pub use relay::RelayState;
pub use sidecars::write_json_atomic;
pub use watchdog::ValidPacketWatchdog;
pub use wfb_rx::WfbRxManager;
