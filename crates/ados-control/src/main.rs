//! `ados-control` daemon — the native HTTP control surface.
//!
//! The runnable daemon. Binds the trusted local Unix socket and the LAN TCP
//! port, serves the shared `/api/*` (+ `/healthz`) Router on each, and shuts
//! down cleanly on `SIGTERM`/`SIGINT`. It answers the agent's control API with
//! no Python runtime.
//!
//! Modeled on the sibling daemons: journald logging on Linux with an fmt
//! fallback off Linux or outside a journald unit, and systemd readiness notify.
//! The binary is functional but ships dark — no supervisor registration and no
//! systemd unit enable it yet, so it has no effect at the install layer until
//! that wiring lands. The crate is inert.
//!
//! `ados-control --apply-config-side-effects <old.yaml>` is a one-shot mode for
//! the other config writers: it compares `<old.yaml>` (the document before a
//! write) with the config now on disk, applies the config side effects (enable
//! markers and unit kicks), prints the report as JSON and exits.

fn main() -> std::process::ExitCode {
    ados_control::daemon::run()
}
