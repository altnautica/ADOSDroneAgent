//! batman-adv local wireless mesh for the relay/receiver roles.
//!
//! [`bringup`] is the `ados-batman` bring-up (`ados-groundlink --mesh-bringup`):
//! the secure join, the `bat0` bind and the gateway mode. [`manager`] observes
//! the result: polls batman-adv neighbours/gateways and publishes the
//! `mesh-state.json` snapshot. Identity
//! (`mesh-id` + `psk.key`) and the snapshot writer are in their own modules;
//! the batctl parsers are pure and unit-tested.
//!
//! On a relay with no delivered identity the manager surfaces
//! [`identity::MeshIdentityError::Missing`] so the caller downgrades to `direct`
//! rather than crash-looping (it asks the supervisor for `direct`).

pub mod batctl;
pub mod bringup;
pub mod gateway;
pub mod identity;
pub mod iface;
pub mod manager;
pub mod secure_join;
pub mod state;

pub use identity::{ensure_mesh_identity, MeshIdentity, MeshIdentityError};
pub use manager::{get_current_role, run_poll_loop};
pub use state::{MeshGateway, MeshNeighbor, MeshSnapshot};

#[cfg(test)]
mod tests;
