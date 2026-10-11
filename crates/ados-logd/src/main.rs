//! `ados-logd` daemon entry point. The runtime lives in [`ados_logd::daemon`].

// Use mimalloc as the global allocator. The daemon is long-running and its
// workload — a constant stream of short-lived read-only SQLite connections
// served off the blocking pool plus the steady ingest and hardware-sample
// churn — fragments the system allocator, which grows per-thread heap arenas it
// then keeps resident. mimalloc bounds the fragmentation and returns freed pages
// to the OS, which holds the daemon's resident set down over a long uptime.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> std::process::ExitCode {
    ados_logd::daemon::run()
}
