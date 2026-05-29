//! `ados-video`: the drone video-pipeline orchestrator.
//!
//! Owns the lifecycle of the C media subprocesses (the `ffmpeg` encoder +
//! RTSP-publish bridge, `mediamtx`, the `wfb_tee` ffmpeg that copies RTSP →
//! RTP UDP 5600) — it supervises them, it does not rewrite them. The Python
//! predecessor (`services/video/`) forked these with `start_new_session=True`
//! and reaped them by process group; a bare single-PID kill once orphaned the
//! bridge ffmpeg onto the mediamtx `/main` publisher slot, so two publishers
//! fought and the video went black. This crate makes that ownership a Rust
//! RAII invariant: every child is its own process-group leader and the whole
//! group is torn down (SIGTERM → wait → SIGKILL) on drop, with a `pgrep` sweep
//! for any straggler from a prior crashed run.
//!
//! The camera HAL probing (v4l2/rpicam parsing) and the FastAPI video routes
//! stay Python; this crate is the long-running supervisor that those surfaces
//! read through the sidecar files + the local RTSP server.

pub mod process;
