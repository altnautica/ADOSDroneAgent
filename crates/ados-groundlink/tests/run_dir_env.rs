//! The `ADOS_RUN_DIR` env-override read path for the Contract-E run-dir paths.
//!
//! This test genuinely exercises the process-global env read in
//! [`ados_groundlink::paths::run_path`], so it mutates `ADOS_RUN_DIR`. It lives
//! in its own integration binary — a separate process from the crate's unit
//! tests and from every other integration binary — so the mutation cannot race
//! any other test thread. The unit-test sidecars all thread an explicit path
//! (the `write_to` / `emit_to` seams) and never touch the env.

use ados_groundlink::paths::run_path;

#[test]
fn run_path_honours_env_override() {
    // Sole test in this binary → sole thread reading/writing the env in this
    // process, so no lock or serialization is required.
    std::env::set_var("ADOS_RUN_DIR", "/tmp/ados-test-run");
    assert_eq!(
        run_path("wfb-stats.json"),
        "/tmp/ados-test-run/wfb-stats.json"
    );

    std::env::remove_var("ADOS_RUN_DIR");
    assert_eq!(run_path("wfb-stats.json"), "/run/ados/wfb-stats.json");
}
