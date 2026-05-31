//! Local-radio auto-pair: drive the first-boot WFB bind IN-PROCESS.
//!
//! This lives in the supervisor, not the cloud relay where it used to run. The
//! bind FSM stops + starts the `ados-wfb` unit to flip wfb-ng profiles, so the
//! trigger cannot live in the radio service (it would kill itself). The earlier
//! home was `ados-cloud` only because "the cloud relay does not touch the
//! radio" — but that is a LAN-only function parked in a service that is idle in
//! local-first mode (cloud relay off by default). The supervisor is the unit
//! that does the stopping (never stopped itself), already owns the
//! [`BindOrchestrator`], and always runs, so it is the correct host. The bind
//! runs in-process via the orchestrator directly, with no control-socket
//! round-trip.
//!
//! Each tick, while armed (`video.wfb.auto_pair_enabled`) and not already paired
//! (the role's key file is absent), run one bind. A successful bind writes the
//! key file, so the next tick sees the rig as paired and stops attempting; the
//! key-apply step also flips `auto_pair_enabled` to false in the config.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::bind::orchestrator::{BindOrchestrator, BindStartError};
use crate::bind::BindRole;

/// The operator config the arm flag is read from each tick.
const CONFIG_YAML: &str = "/etc/ados/config.yaml";
/// Settle delay before the first attempt (lets the radio + units come up).
pub const START_DELAY: Duration = Duration::from_secs(15);
/// Backoff between attempts.
pub const RETRY_BACKOFF: Duration = Duration::from_secs(60);

/// Whether auto-pair should attempt a bind this tick. Pure for testing.
pub fn should_attempt(armed: bool, already_paired: bool) -> bool {
    armed && !already_paired
}

/// Parse `video.wfb.auto_pair_enabled` out of a config body. Default TRUE: a
/// fresh rig auto-binds on first boot unless the operator has explicitly
/// disarmed it, and a successful bind disarms it. An absent `video.wfb` section
/// therefore reads as armed, which also keeps this in step with the Python
/// config model's default (the two diverged before, so the status display read
/// "armed" while the supervisor was silently disarmed).
pub fn read_armed_from(text: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct Raw {
        #[serde(default)]
        video: VideoSec,
    }
    #[derive(serde::Deserialize, Default)]
    struct VideoSec {
        #[serde(default)]
        wfb: Option<WfbSec>,
    }
    #[derive(serde::Deserialize)]
    struct WfbSec {
        #[serde(default = "default_true")]
        auto_pair_enabled: bool,
    }
    fn default_true() -> bool {
        true
    }
    match serde_norway::from_str::<Raw>(text) {
        Ok(raw) => raw.video.wfb.map(|w| w.auto_pair_enabled).unwrap_or(true),
        Err(_) => true,
    }
}

/// Read the arm flag off disk; any read/parse failure keeps the default-armed
/// posture so a transient fs hiccup never silently disables first-boot pairing.
fn read_armed() -> bool {
    match std::fs::read_to_string(CONFIG_YAML) {
        Ok(text) => read_armed_from(&text),
        Err(_) => true,
    }
}

/// True when this role's key file already exists (the rig is paired).
fn already_paired(role: BindRole) -> bool {
    Path::new(role.key_path()).exists()
}

/// Run the auto-pair loop until `shutdown` flips. Drives the bind in-process via
/// the shared orchestrator.
pub async fn run(orch: Arc<BindOrchestrator>, role: BindRole, mut shutdown: watch::Receiver<bool>) {
    tracing::info!(role = role.as_str(), "auto_pair_supervisor_started");
    // Settle before the first attempt.
    tokio::select! {
        _ = shutdown.changed() => return,
        _ = tokio::time::sleep(START_DELAY) => {}
    }
    loop {
        if *shutdown.borrow() {
            break;
        }
        let armed = read_armed();
        let paired = already_paired(role);
        if should_attempt(armed, paired) {
            // Tear down an in-flight bind if the supervisor shuts down.
            let mut cancel_rx = shutdown.clone();
            let cancel = async move {
                let _ = cancel_rx.changed().await;
            };
            match orch.start_local_bind(role, None, "auto", cancel).await {
                Ok(session) => {
                    let state = session.get("state").and_then(|s| s.as_str()).unwrap_or("");
                    if state == "paired" {
                        tracing::info!(role = role.as_str(), "auto_pair_bind_completed");
                    } else {
                        tracing::info!(role = role.as_str(), state, "auto_pair_attempt_unpaired");
                    }
                }
                Err(BindStartError::Busy) => {
                    tracing::info!("auto_pair_busy_retry");
                }
            }
        }
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = tokio::time::sleep(RETRY_BACKOFF) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_attempt_gate() {
        assert!(should_attempt(true, false));
        assert!(!should_attempt(false, false)); // disarmed
        assert!(!should_attempt(true, true)); // already paired
    }

    #[test]
    fn armed_defaults_true_without_wfb_section() {
        // A fresh config with no `video.wfb` still arms (first-boot auto-bind).
        assert!(read_armed_from("video:\n  mode: auto\n"));
        assert!(read_armed_from("agent:\n  profile: drone\n"));
        assert!(read_armed_from(""));
    }

    #[test]
    fn armed_honors_explicit_disarm() {
        assert!(!read_armed_from(
            "video:\n  wfb:\n    auto_pair_enabled: false\n"
        ));
        assert!(read_armed_from(
            "video:\n  wfb:\n    auto_pair_enabled: true\n"
        ));
    }
}
