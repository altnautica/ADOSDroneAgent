//! The auto-pair loop each rig runs in the background, after the operators
//! are done: the production decisions (`auto_pair::bind_this_tick`, the
//! `pair_proof` re-arm latch with its holds and cooldown, the drone and ground
//! station signal rules) fed from the simulated radio. A rig that is armed and
//! unpaired binds on every tick; a rig holding a key that never carried a link
//! re-arms once the fault has held for the latch's confirm window, and so does
//! a rig whose key worked once but whose peer is on the air with another key.
//! This is what reconciles a pair a fault split in its last exchange. A bind
//! window opening while the pair decodes is a break.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::pair_proof::{write_pair_proof_to, PairProof};
use ados_supervisor::auto_pair::{bind_this_tick, RETRY_BACKOFF, RETRY_BACKOFF_JITTER_SECS};
use ados_supervisor::bind::orchestrator::{BindOrchestrator, BindStartError};
use ados_supervisor::bind::BindRole;
use ados_supervisor::pair_proof::machine::{
    drone_signals, gs_signals, RearmSignals, DEFAULT_REARM_COOLDOWN_S, REARM_CONFIRM_HOLD,
};
use ados_supervisor::pair_proof::{LatchInputs, PairProofLatch, PairRearmConfig};
use tokio::time::Instant;

use crate::check;
use crate::world::{key_origin, lock, Rig, Shared, World};

/// How long a split may take to reconcile: the latch's confirm hold, one
/// cooldown for a re-arm window that failed, and two bind windows' slack.
pub fn bound() -> Duration {
    REARM_CONFIRM_HOLD + Duration::from_secs(DEFAULT_REARM_COOLDOWN_S) + Duration::from_secs(600)
}

/// The latch identity of a key file.
pub fn key_fingerprint(k: &[u8]) -> String {
    hex::encode(&k[32..40])
}

/// The latch identity of a rig's key.
pub fn fingerprint(r: &Rig) -> Option<String> {
    r.key().filter(|k| k.len() == 64).map(key_fingerprint)
}

/// Seed a pair-proof record: the key `fp` carried a link once.
pub fn seed_proven(record: &Path, role: BindRole, fp: &str, now_unix: u64) {
    let mut proof = PairProof::fresh(role.as_str(), fp);
    proof.mark_proven(now_unix);
    write_pair_proof_to(record, &proof).expect("a pair-proof record");
}

/// The fresh verdict the radio's stats sidecar would carry; `None` while the
/// normal radio unit (its writer) is stopped.
fn signals(w: &World, rig: usize) -> Option<RearmSignals> {
    let r = &w.rigs[rig];
    if !r.unit_running(r.role.normal_unit()) {
        return None;
    }
    let link = w.radio_link();
    let peer = &w.rigs[1 - rig];
    // The peer transmits on a key this rig does not hold: its frames arrive
    // and fail decryption (`link_diag` `mis_keyed`). A peer with no key at all
    // keeps its radio parked and sends nothing.
    let mis_keyed = !link
        && !w.outage()
        && r.key().is_some()
        && peer.unit_running(peer.role.normal_unit())
        && peer.key().is_some()
        && peer.key().and_then(key_origin).map(|(g, _)| g)
            != r.key().and_then(key_origin).map(|(g, _)| g);
    let sig = match r.role {
        // A drone transmits only with a key, so only then can it read unverified.
        BindRole::Drone => drone_signals(Some(r.key().is_some() && !link), Some(link)),
        BindRole::Gs => {
            let state = match (r.key().is_some(), link) {
                (false, _) => "blocked_unpaired",
                (true, true) => "receiving",
                (true, false) => "searching",
            };
            gs_signals(state, u64::from(link))
        }
    };
    Some(sig.with_link_diag(Some(if mis_keyed { "mis_keyed" } else { "searching" })))
}

/// Both rigs decode each other and neither is binding.
pub fn converged(w: &World) -> bool {
    w.radio_link() && w.rigs.iter().all(|r| !r.busy)
}

/// One rig's auto-pair loop until `deadline`.
pub async fn auto_pair(
    w: Shared,
    rig: usize,
    orch: Arc<BindOrchestrator>,
    record: PathBuf,
    deadline: Instant,
) {
    let role = lock(&w).rigs[rig].role;
    let mut latch = PairProofLatch::with_path(role, record);
    while Instant::now() < deadline {
        let (armed, fingerprint, sig, now_std, now_unix) = {
            let g = lock(&w);
            let r = &g.rigs[rig];
            let now_unix = g
                .wall_now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            (
                check::armed(r),
                fingerprint(r),
                signals(&g, rig),
                g.std_now(),
                now_unix,
            )
        };
        let paired = fingerprint.is_some();
        let out = latch.step(
            LatchInputs {
                fingerprint,
                signals: sig,
                session_active: orch.session_active(),
                cfg: PairRearmConfig::default(),
            },
            now_std,
            now_unix,
        );
        if let Some(event) = &out.event {
            let mut g = lock(&w);
            let name = g.rigs[rig].name;
            g.log(format!("{name} pair re-arm latch: {event:?}"));
        }
        // The bench FC is disarmed and the adapter present.
        if bind_this_tick(armed, paired, out.rearm, Some(false), true) {
            {
                let mut g = lock(&w);
                let name = g.rigs[rig].name;
                if g.radio_link() {
                    let at_ms = g.now_ms();
                    g.violations.push(format!(
                        "{name}: a bind window opened on a decoding link at {at_ms} ms"
                    ));
                }
                g.rigs[rig].busy = true;
                g.rigs[rig].last_bind_key = g.rigs[rig].key().map(<[u8]>::to_vec);
                g.log(format!("{name} auto-pair bind (re-arm: {})", out.rearm));
            }
            let res = orch
                .start_local_bind(role, None, "auto", None, std::future::pending())
                .await;
            let mut g = lock(&w);
            let r = &mut g.rigs[rig];
            r.busy = false;
            let name = r.name;
            let line = match res {
                Ok(v) => {
                    let state = v["state"].as_str().unwrap_or("").to_string();
                    r.last_state = Some(state.clone());
                    format!("{name} auto-pair bind ends {state}")
                }
                Err(BindStartError::Busy) => format!("{name} auto-pair bind refused busy"),
            };
            g.log(line);
        }
        let jitter = {
            let mut g = lock(&w);
            Duration::from_secs(g.rng.next_u64() % (RETRY_BACKOFF_JITTER_SECS + 1))
        };
        tokio::time::sleep(RETRY_BACKOFF + jitter).await;
    }
}
