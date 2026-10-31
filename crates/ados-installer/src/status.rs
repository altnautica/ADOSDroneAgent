//! `--status`: the recorded install result, completed checkpoints and the
//! per-binary presence for the resolved profile.

use crate::binaries;
use crate::checkpoint::Checkpoint;
use crate::cli::Args;
use crate::env::{self, RESULT_PATH};

/// Print install status: the install-result contract (if present), the
/// completed checkpoints, and the per-binary presence for the resolved profile.
pub fn print_status(args: &Args) {
    // 1. The install-result contract.
    match std::fs::read_to_string(RESULT_PATH) {
        Ok(body) => {
            println!("install-result ({RESULT_PATH}):");
            print!("{body}");
            if !body.ends_with('\n') {
                println!();
            }
        }
        Err(_) => {
            println!("install-result: none at {RESULT_PATH} (no install recorded yet)");
        }
    }

    // 2. Completed checkpoints.
    let checkpoint = Checkpoint::new();
    let done = checkpoint.list();
    if done.is_empty() {
        println!("\ncheckpoints: none");
    } else {
        println!("\ncheckpoints completed:");
        for cp in &done {
            println!("  [x] {cp}");
        }
    }

    // 3. Per-binary presence for the profile. The profile flag wins; else the
    // persisted profile.conf; else drone (matches preflight's resolution).
    let profile = resolve_status_profile(args);
    println!("\nprebuilt binaries ({profile}):");
    for b in binaries::for_profile(&profile) {
        let present = std::fs::metadata(b.dest)
            .map(|m| m.is_file())
            .unwrap_or(false);
        let mark = if present { "[x]" } else { "[ ]" };
        let gate = match b.gate {
            binaries::Gate::Hard => "hard",
            binaries::Gate::BestEffort => "best-effort",
        };
        println!("  {mark} {} ({gate})", b.service);
    }
}

/// Resolve the profile for `--status` reporting: `--profile` flag, else the
/// persisted `profile.conf`, else `drone`. Reuses preflight's pure resolver.
fn resolve_status_profile(args: &Args) -> String {
    let conf_body = std::fs::read_to_string(env::PROFILE_CONF).ok();
    crate::steps::preflight::resolve_profile(args.profile.as_deref(), conf_body.as_deref())
}
