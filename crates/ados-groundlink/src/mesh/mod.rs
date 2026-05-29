//! batman-adv local wireless mesh for the relay/receiver roles.
//!
//! Ports `mesh_manager.py`: brings up a second wireless interface in 802.11s or
//! IBSS mode bound to `bat0`, drives batman-adv gateway mode from role + cloud
//! uplink, polls neighbors/gateways, and publishes the `mesh-state.json`
//! snapshot. Identity (`mesh-id` + `psk.key`) and the snapshot writer are in
//! their own modules; the batctl parsers + gateway-mode logic are pure and
//! unit-tested.
//!
//! On a relay with no delivered identity the manager surfaces
//! [`identity::MeshIdentityError::Missing`] so the caller downgrades to `direct`
//! rather than crash-looping (matching the Python graceful-downgrade path).

pub mod batctl;
pub mod identity;
pub mod manager;
pub mod state;

pub use identity::{ensure_mesh_identity, MeshIdentity, MeshIdentityError};
pub use manager::{get_current_role, run_poll_loop};
pub use state::{MeshGateway, MeshNeighbor, MeshSnapshot};
