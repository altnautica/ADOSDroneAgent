//! Universal setup REST surface for the lightweight ADOS Drone Agent.
//!
//! Implements every route documented in `proto/setup/setup-api.yaml` and
//! returns response shapes byte-for-byte compatible with the Python
//! reference implementation in `src/ados/api/routes/setup.py`. After this
//! crate ships, a Luckfox board running ONLY the Rust lite agent (no
//! Python venv) can complete the full setup wizard locally — profile
//! selection, hardware-check, cloud-choice, finalize, step-skip,
//! Cloudflare Tunnel quick-install, log streaming, reset.
//!
//! Module layout:
//! - `state`     — atomic-write persistence for setup-state.yaml + pairing.json
//! - `models`    — request and response shapes mirroring the Python Pydantic models
//! - `profile`   — apply profile + ground role to agent.yaml
//! - `cloud`     — apply cloud choice (cloud / self_hosted / local) to agent.yaml
//! - `hardware`  — board.yaml + /proc + lsusb fingerprint engine
//! - `cloudflare` — Cloudflare Tunnel orchestration + WebSocket log stream
//! - `handlers`  — axum handler functions
//! - `router`    — assemble axum::Router with all 11 routes

#![forbid(unsafe_code)]

pub mod atomic;
pub mod cloud;
pub mod cloudflare;
pub mod handlers;
pub mod hardware;
pub mod models;
pub mod pairing;
pub mod profile;
pub mod router;
pub mod state;
pub mod webapp;

pub use router::{setup_router, SetupState};
