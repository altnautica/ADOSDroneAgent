//! Shared low-level primitives for the lightweight ADOS Drone Agent.
//!
//! This crate hosts utilities that more than one workspace member needs
//! and that have no other natural home. Today that is just the atomic
//! file-write helper; future entries will likely include shared path
//! constants and the IPC framing primitives.
//!
//! Keep dependencies here minimal — every transitive dep added to
//! `ados-core` is paid for by every consumer crate.

#![forbid(unsafe_code)]

pub mod atomic;
