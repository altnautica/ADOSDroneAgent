//! Latch tests. Every one is hardware-free and clock-free: the wall clock, the
//! monotonic clock, the radio verdict and the key fingerprint are all injected,
//! and the only thing touched on disk is a temp record file.

use super::*;
use ados_protocol::pair_proof::read_pair_proof_from;

const FP: &str = "aabbccdd11223344";
const OTHER_FP: &str = "9988776655443322";

/// Signals for a rig holding a key that plainly does not work.
fn unproven() -> Option<RearmSignals> {
    Some(RearmSignals {
        unproven: true,
        proven: false,
    })
}

/// Signals for a rig whose key was just seen carrying a link.
fn proven() -> Option<RearmSignals> {
    Some(RearmSignals {
        unproven: false,
        proven: true,
    })
}

/// A rig with a radio that is up but has nothing to say either way.
fn quiet_signals() -> Option<RearmSignals> {
    Some(RearmSignals::default())
}

struct Rig {
    latch: PairProofLatch,
    path: PathBuf,
    cfg: PairRearmConfig,
    t0: Instant,
    unix0: u64,
    _dir: tempfile::TempDir,
}

impl Rig {
    fn new(role: BindRole) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wfb-pair-proof.json");
        Self {
            latch: PairProofLatch::with_path(role, path.clone()),
            path,
            cfg: PairRearmConfig::default(),
            t0: Instant::now(),
            unix0: 1_700_000_000,
            _dir: dir,
        }
    }

    /// One tick at `offset` seconds after the start, with the given signals.
    fn tick(&mut self, offset_s: u64, signals: Option<RearmSignals>) -> LatchOutcome {
        self.tick_as(offset_s, signals, Some(FP.to_string()), false)
    }

    fn tick_as(
        &mut self,
        offset_s: u64,
        signals: Option<RearmSignals>,
        fingerprint: Option<String>,
        session_active: bool,
    ) -> LatchOutcome {
        self.latch.step(
            LatchInputs {
                fingerprint,
                signals,
                session_active,
                cfg: self.cfg.clone(),
            },
            self.t0 + Duration::from_secs(offset_s),
            self.unix0 + offset_s,
        )
    }

    /// Feed the unproven condition from `from_s` until the confirm hold is
    /// served, returning every outcome. Ticks once a minute, like the real loop
    /// only coarser.
    fn serve_hold(&mut self, from_s: u64) -> Vec<LatchOutcome> {
        let hold = self.cfg.confirm_hold.as_secs();
        let mut out = Vec::new();
        let mut t = from_s;
        while t <= from_s + hold {
            out.push(self.tick(t, unproven()));
            t += 60;
        }
        out
    }

    /// Rebuild the latch on the same record file: everything in memory is lost,
    /// everything persisted survives. A reboot, in other words.
    fn reboot(&mut self, role: BindRole) {
        self.latch = PairProofLatch::with_path(role, self.path.clone());
    }

    fn stored(&self) -> Option<PairProof> {
        read_pair_proof_from(&self.path)
    }
}

fn arms(out: &[LatchOutcome]) -> usize {
    out.iter().filter(|o| o.rearm).count()
}

// ── the headline property ────────────────────────────────────────────────────

#[test]
fn healthy_link_is_never_rearmed() {
    // A pair that has worked before. Its link then goes down and STAYS down for
    // a full day, with the radio reporting the exact "transmitting, zero
    // confirmed reception" condition that recovers a stale key.
    //
    // It must do nothing. Not one bind window, not one spent episode, not one
    // event. Silently re-binding a proven pair would be a worse failure than the
    // deadlock this latch exists to fix, so this is the property under the most
    // pressure: a transient (or multi-day) dropout has to be structurally
    // incapable of triggering a re-bind, not merely unlikely to.
    let mut rig = Rig::new(BindRole::Drone);
    let mut seed = PairProof::fresh("drone", FP);
    seed.mark_proven(rig.unix0 - 5_000);
    write_pair_proof_to(&rig.path, &seed).unwrap();

    let mut events = 0usize;
    let mut rearms = 0usize;
    // 24 hours at one tick a minute.
    for minute in 0..(24 * 60u64) {
        let out = rig.tick(minute * 60, unproven());
        assert_eq!(
            out.step,
            RearmStep::Proven,
            "minute {minute} must stay latched"
        );
        if out.rearm {
            rearms += 1;
        }
        if out.event.is_some() {
            events += 1;
        }
    }
    assert_eq!(rearms, 0, "a proven key must never re-arm");
    assert_eq!(events, 0, "a healthy rig must be silent");

    let stored = rig.stored().unwrap();
    assert_eq!(stored.rearm_episodes, 0, "no budget may be spent");
    assert_eq!(stored.last_rearm_at, None);
    assert_eq!(
        stored.proven_at, seed.proven_at,
        "the proof stamp is not churned on a flash card"
    );
}

#[test]
fn one_proof_tick_locks_out_rearm_permanently_for_that_key() {
    // The transition itself: a key that was arming its way toward a re-bind is
    // seen working ONCE, and is never re-armed again.
    let mut rig = Rig::new(BindRole::Drone);
    // Half-way through the hold.
    let half = rig.cfg.confirm_hold.as_secs() / 2;
    rig.tick(0, unproven());
    rig.tick(half, unproven());
    assert!(!rig.stored().is_some_and(|p| p.is_proven()));

    // One proof.
    let out = rig.tick(half + 60, proven());
    assert_eq!(out.event, Some(RearmEvent::Proven));
    assert!(rig.stored().unwrap().is_proven());

    // Now hold the fault for a full day: nothing.
    for minute in 0..(24 * 60u64) {
        let out = rig.tick(half + 120 + minute * 60, unproven());
        assert!(!out.rearm);
        assert_eq!(out.step, RearmStep::Proven);
        assert_eq!(out.event, None);
    }
}

// ── the recovery it exists for ───────────────────────────────────────────────

#[test]
fn a_never_proven_key_arms_exactly_once_at_the_hold_boundary() {
    // The live failure: a structurally valid key from a peer that was reflashed.
    // The fingerprint never entered the record, so the latch may recover it —
    // once, at the boundary, and then not again until the cooldown.
    let mut rig = Rig::new(BindRole::Drone);
    let hold = rig.cfg.confirm_hold.as_secs();

    // Everything strictly inside the window declines.
    let mut t = 0;
    while t < hold {
        let out = rig.tick(t, unproven());
        assert!(
            !out.rearm,
            "armed at {t}s, before the {hold}s hold was served"
        );
        assert_eq!(out.step, RearmStep::Idle);
        t += 60;
    }
    // The boundary arms.
    let out = rig.tick(hold, unproven());
    assert!(out.rearm);
    assert_eq!(
        out.step,
        RearmStep::Arm {
            episode: 1,
            forced: false
        }
    );
    assert_eq!(
        out.event,
        Some(RearmEvent::Armed {
            episode: 1,
            forced: false
        })
    );

    // And the episode is spent immediately, not on success — a bind that fails
    // is exactly the attempt the budget must count.
    let stored = rig.stored().unwrap();
    assert_eq!(stored.rearm_episodes, 1);
    assert_eq!(stored.last_rearm_at, Some(rig.unix0 + hold));

    // The fault keeps holding: no second window until the cooldown elapses.
    let mut more = Vec::new();
    for i in 1..30u64 {
        more.push(rig.tick(hold + i * 60, unproven()));
    }
    assert_eq!(arms(&more), 0);
    assert!(more
        .iter()
        .all(|o| matches!(o.step, RearmStep::Cooldown { .. })));
    assert!(more.iter().all(|o| o.event.is_none()));
}

#[test]
fn a_new_fingerprint_resets_the_record_and_the_debounce() {
    // A successful re-bind writes a new key. It must start clean: no inherited
    // proof, no spent budget, and no inherited fault — the previous key's
    // accumulated hold must not arm a brand-new key on its first tick.
    let mut rig = Rig::new(BindRole::Drone);
    let hold = rig.cfg.confirm_hold.as_secs();
    rig.serve_hold(0);
    assert_eq!(rig.stored().unwrap().rearm_episodes, 1);

    // The key changes.
    let out = rig.tick_as(hold + 60, unproven(), Some(OTHER_FP.to_string()), false);
    assert_eq!(out.event, Some(RearmEvent::Cleared));
    assert!(!out.rearm);
    let stored = rig.stored().unwrap();
    assert_eq!(stored.key_fingerprint, OTHER_FP);
    assert_eq!(stored.rearm_episodes, 0);
    assert_eq!(stored.last_rearm_at, None);

    // The debounce restarted with the key: the new key must serve its own full
    // hold before anything happens.
    let mut t = hold + 120;
    while t < hold + 120 + hold {
        let out = rig.tick_as(t, unproven(), Some(OTHER_FP.to_string()), false);
        assert!(
            !out.rearm,
            "the new key inherited the old key's hold at {t}s"
        );
        t += 60;
    }
}

// ── bounds ───────────────────────────────────────────────────────────────────

#[test]
fn a_transient_gap_under_the_hold_does_not_arm_and_restarts_the_full_hold() {
    let mut rig = Rig::new(BindRole::Drone);
    let hold = rig.cfg.confirm_hold.as_secs();

    // Almost there, then the fault clears for one tick.
    let mut t = 0;
    while t < hold - 60 {
        assert!(!rig.tick(t, unproven()).rearm);
        t += 60;
    }
    assert!(!rig.tick(t, quiet_signals()).rearm);

    // A fresh onset must serve the FULL hold again, not the one tick it was short.
    let onset = t + 60;
    let mut t = onset;
    while t < onset + hold {
        assert!(
            !rig.tick(t, unproven()).rearm,
            "armed at {t}s on a hold that restarted at {onset}s"
        );
        t += 60;
    }
    assert!(rig.tick(onset + hold, unproven()).rearm);
}

#[test]
fn the_cooldown_survives_a_reboot() {
    // A rig that crash-loops must not get a fresh cooldown on every boot: the
    // anchor is the persisted wall clock, not an in-process timer.
    let mut rig = Rig::new(BindRole::Drone);
    let hold = rig.cfg.confirm_hold.as_secs();
    rig.serve_hold(0);
    assert_eq!(rig.stored().unwrap().rearm_episodes, 1);

    // Reboot, then serve a whole new hold from a latch with no memory.
    rig.reboot(BindRole::Drone);
    let start = hold + 60;
    let out = rig.serve_hold(start);
    assert_eq!(arms(&out), 0, "a reboot must not shorten the cooldown");
    assert!(out
        .last()
        .is_some_and(|o| matches!(o.step, RearmStep::Cooldown { .. })));

    // Past the cooldown, the second episode is allowed.
    let after = rig.cfg.cooldown_s + 120;
    rig.reboot(BindRole::Drone);
    let out = rig.serve_hold(after);
    assert_eq!(arms(&out), 1);
    assert_eq!(rig.stored().unwrap().rearm_episodes, 2);
}

#[test]
fn the_episode_budget_is_bounded_and_persists_across_reboots() {
    let mut rig = Rig::new(BindRole::Drone);
    let max = rig.cfg.max_episodes;
    let cooldown = rig.cfg.cooldown_s;
    let hold = rig.cfg.confirm_hold.as_secs();

    let mut total_arms = 0usize;
    let mut t = 0u64;
    // Far more chances than the budget allows, each after a reboot and a full
    // cooldown, so nothing but the budget itself can be what stops it.
    for _ in 0..(max + 5) {
        rig.reboot(BindRole::Drone);
        total_arms += arms(&rig.serve_hold(t));
        t += hold + cooldown + 120;
    }
    assert_eq!(total_arms, max as usize);
    assert_eq!(rig.stored().unwrap().rearm_episodes, max);

    // And it parks loudly, once.
    rig.reboot(BindRole::Drone);
    let out = rig.serve_hold(t);
    let last = out.last().unwrap();
    assert_eq!(last.step, RearmStep::Exhausted);
    let exhausted_events = out
        .iter()
        .filter(|o| matches!(o.event, Some(RearmEvent::Exhausted { .. })))
        .count();
    assert_eq!(exhausted_events, 1, "exhausted is a level, announced once");
}

// ── things that must not be read as a verdict ────────────────────────────────

#[test]
fn a_stale_sidecar_is_not_a_signal_and_resets_the_hold() {
    // A stopped radio leaves its last flags frozen. The reader turns that into
    // `None`, and `None` must RESET the accumulated hold rather than freeze it —
    // otherwise a radio that flaps in and out of reporting eventually accumulates
    // a full window it never actually held.
    let mut rig = Rig::new(BindRole::Drone);
    let hold = rig.cfg.confirm_hold.as_secs();

    let mut t = 0;
    while t < hold - 60 {
        rig.tick(t, unproven());
        t += 60;
    }
    // The radio stops; the sidecar goes stale.
    let out = rig.tick(t, None);
    assert!(!out.rearm);
    assert_eq!(out.step, RearmStep::Idle);

    // Signals come back. The hold restarts from here.
    let onset = t + 60;
    let mut t = onset;
    while t < onset + hold {
        assert!(
            !rig.tick(t, unproven()).rearm,
            "stale froze the hold at {t}s"
        );
        t += 60;
    }
    assert!(rig.tick(onset + hold, unproven()).rearm);
}

#[test]
fn an_idle_radio_never_accumulates_hold() {
    // No transmit, no verdict: a radio that is up but not injecting reports
    // neither flag, and a day of that must arm nothing.
    let mut rig = Rig::new(BindRole::Drone);
    let mut out = Vec::new();
    for minute in 0..(24 * 60u64) {
        out.push(rig.tick(minute * 60, quiet_signals()));
    }
    assert_eq!(arms(&out), 0);
    assert!(out.iter().all(|o| o.event.is_none()));
    assert_eq!(rig.stored(), None, "nothing to persist, nothing written");
}

#[test]
fn a_bind_in_flight_suspends_the_trigger() {
    // A bind window is `rf_unverified` by construction, so a running bind would
    // feed the very trigger that opens one.
    let mut rig = Rig::new(BindRole::Drone);
    let hold = rig.cfg.confirm_hold.as_secs();
    let mut t = 0;
    // Twice the hold, entirely inside a bind session.
    while t <= hold * 2 {
        let out = rig.tick_as(t, unproven(), Some(FP.to_string()), true);
        assert!(!out.rearm, "armed at {t}s while a bind was running");
        t += 60;
    }
    // The session ends; the hold starts from there, not from the accumulated
    // session time.
    let onset = t;
    while t < onset + hold {
        assert!(!rig.tick(t, unproven()).rearm);
        t += 60;
    }
    assert!(rig.tick(onset + hold, unproven()).rearm);
}

#[test]
fn an_unpaired_rig_is_left_to_ordinary_auto_pair() {
    // No complete key on disk is not "paired but broken"; auto-pair is already
    // arming for it, and the latch must neither arm nor write a record.
    let mut rig = Rig::new(BindRole::Drone);
    let hold = rig.cfg.confirm_hold.as_secs();
    let mut t = 0;
    while t <= hold * 2 {
        let out = rig.tick_as(t, unproven(), None, false);
        assert!(!out.rearm);
        assert_eq!(out.event, None);
        t += 60;
    }
    assert_eq!(rig.stored(), None);
}

#[test]
fn a_wrong_profile_sidecar_is_ignored() {
    // The two planes measure different things and neither rule transfers, so a
    // sidecar from the other profile is no verdict rather than a misread one.
    let drone_body = serde_json::json!({
        "profile": "drone", "rf_unverified": true, "channel_locked": false
    });
    let gs_body = serde_json::json!({
        "profile": "ground_station", "state": "searching", "packets_received": 0
    });
    assert!(
        parse_signals(BindRole::Drone, &drone_body)
            .unwrap()
            .unproven
    );
    assert!(parse_signals(BindRole::Gs, &gs_body).unwrap().unproven);
    assert_eq!(parse_signals(BindRole::Drone, &gs_body), None);
    assert_eq!(parse_signals(BindRole::Gs, &drone_body), None);
    // A body with no profile at all is no verdict either.
    assert_eq!(
        parse_signals(BindRole::Drone, &serde_json::json!({"rf_unverified": true})),
        None
    );
}

#[test]
fn the_ground_station_counts_searching_but_not_blocked_unpaired() {
    // `searching` means the key is present, the chain is running and nothing
    // decodes — a verdict on the key. `blocked_unpaired` means the chain never
    // ran, so there is no verdict at all.
    let searching = serde_json::json!({
        "profile": "ground_station", "state": "searching", "packets_received": 0
    });
    let blocked = serde_json::json!({
        "profile": "ground_station", "state": "blocked_unpaired", "packets_received": 0
    });
    assert!(parse_signals(BindRole::Gs, &searching).unwrap().unproven);
    assert!(!parse_signals(BindRole::Gs, &blocked).unwrap().unproven);

    // And a ground station that decodes is proven, whatever its state string.
    let decoding = serde_json::json!({
        "profile": "ground_station", "state": "active", "packets_received": 41
    });
    assert!(parse_signals(BindRole::Gs, &decoding).unwrap().proven);
}

#[test]
fn a_ground_station_recovers_a_stale_key_the_same_way() {
    // The whole latch, driven by the ground plane's own signals.
    let mut rig = Rig::new(BindRole::Gs);
    let hold = rig.cfg.confirm_hold.as_secs();
    let searching = serde_json::json!({
        "profile": "ground_station", "state": "searching", "packets_received": 0
    });
    let sig = parse_signals(BindRole::Gs, &searching);

    let mut t = 0;
    while t < hold {
        assert!(!rig.tick(t, sig).rearm);
        t += 60;
    }
    assert!(rig.tick(hold, sig).rearm);
    assert_eq!(rig.stored().unwrap().role, "gs");
}

// ── the operator escape hatch ────────────────────────────────────────────────

#[test]
fn force_arms_once_then_self_clears() {
    // The hatch exists because the only prior way to re-arm a paired rig was to
    // unpair it, which DELETES the key — the worst possible move when the key
    // was fine. Force opens one window and leaves the key alone.
    let mut rig = Rig::new(BindRole::Drone);
    let mut seed = PairProof::fresh("drone", FP);
    seed.mark_proven(rig.unix0 - 100); // proven: normally never re-armed
    seed.rearm_episodes = rig.cfg.max_episodes + 2; // and out of budget
    seed.force_rearm = true;
    write_pair_proof_to(&rig.path, &seed).unwrap();

    // One tick, no hold served, no signals at all: it arms.
    let out = rig.tick(0, None);
    assert!(out.rearm);
    assert_eq!(
        out.event,
        Some(RearmEvent::Armed {
            episode: rig.cfg.max_episodes + 3,
            forced: true
        })
    );

    // The flag is consumed, and the key is untouched by any of this.
    let stored = rig.stored().unwrap();
    assert!(!stored.force_rearm);
    assert!(stored.is_proven(), "a force must not erase the proof");
    assert_eq!(stored.key_fingerprint, FP);

    // Every later tick is back to the normal rule.
    for minute in 1..600u64 {
        let out = rig.tick(minute * 60, unproven());
        assert!(!out.rearm, "force re-fired at minute {minute}");
        assert_eq!(out.step, RearmStep::Proven);
    }
}

// ── config + event shape ─────────────────────────────────────────────────────

#[test]
fn an_absent_or_malformed_config_leaves_the_latch_enabled() {
    // A config the agent cannot read must never silently disable a self-heal.
    for text in ["", "agent:\n  profile: drone\n", ": : : not yaml"] {
        let cfg = read_config_from(text);
        assert!(cfg.enabled, "{text:?} disabled the latch");
        assert_eq!(cfg.confirm_hold, REARM_CONFIRM_HOLD);
        assert_eq!(cfg.max_episodes, DEFAULT_MAX_REARM_EPISODES);
        assert_eq!(cfg.cooldown_s, DEFAULT_REARM_COOLDOWN_S);
    }
}

#[test]
fn the_config_tunables_are_honored_and_floored() {
    let cfg = read_config_from(
        "video:\n  wfb:\n    pair_rearm:\n      confirm_hold_s: 120\n      max_episodes: 0\n      cooldown_s: 60\n      stats_fresh_ceiling_s: 5\n",
    );
    assert!(cfg.enabled);
    assert_eq!(cfg.confirm_hold, Duration::from_secs(120));
    // Zero episodes would mean "never recover"; `enabled: false` is how you turn
    // it off, so the floor is one.
    assert_eq!(cfg.max_episodes, 1);
    assert_eq!(cfg.cooldown_s, 60);
    assert_eq!(cfg.stats_fresh_ceiling, Duration::from_secs(5));
}

#[test]
fn an_explicitly_disabled_latch_does_nothing() {
    let mut rig = Rig::new(BindRole::Drone);
    rig.cfg = read_config_from("video:\n  wfb:\n    pair_rearm:\n      enabled: false\n");
    assert!(!rig.cfg.enabled);
    let hold = REARM_CONFIRM_HOLD.as_secs();
    let mut t = 0;
    while t <= hold * 2 {
        let out = rig.tick(t, unproven());
        assert!(!out.rearm);
        assert_eq!(out.event, None);
        t += 60;
    }
    assert_eq!(rig.stored(), None);
}

#[test]
fn the_event_detail_is_bland_and_carries_the_episode() {
    let d = rearm_detail(
        &RearmEvent::Armed {
            episode: 2,
            forced: false,
        },
        "drone",
        FP,
        5,
    );
    assert_eq!(d.get("state").and_then(|v| v.as_str()), Some("armed"));
    assert_eq!(d.get("role").and_then(|v| v.as_str()), Some("drone"));
    assert_eq!(d.get("episode").and_then(|v| v.as_u64()), Some(2));
    assert_eq!(d.get("max_episodes").and_then(|v| v.as_u64()), Some(5));
    assert_eq!(d.get("forced").and_then(|v| v.as_bool()), Some(false));

    // Opening a bind window on a rig that believed it was paired, and giving up
    // on one, are both things an operator should see.
    assert_eq!(
        RearmEvent::Armed {
            episode: 1,
            forced: false
        }
        .level(),
        Level::Warn
    );
    assert_eq!(RearmEvent::Exhausted { episodes: 5 }.level(), Level::Warn);
    assert_eq!(RearmEvent::Proven.level(), Level::Info);
    assert_eq!(RearmEvent::Cleared.level(), Level::Info);
    assert_eq!(RearmEvent::Cleared.state(), "cleared");
}

#[tokio::test]
async fn a_stale_stats_file_reads_as_no_signal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wfb-stats.json");
    std::fs::write(
        &path,
        serde_json::json!({"profile": "drone", "rf_unverified": true}).to_string(),
    )
    .unwrap();

    // Fresh: a verdict.
    assert!(
        read_signals(BindRole::Drone, &path, Duration::from_secs(30))
            .await
            .unwrap()
            .unproven
    );
    // A zero-second ceiling makes any file stale: no verdict.
    assert_eq!(
        read_signals(BindRole::Drone, &path, Duration::ZERO).await,
        None
    );
    // An absent file is no verdict either.
    assert_eq!(
        read_signals(
            BindRole::Drone,
            &dir.path().join("missing.json"),
            Duration::from_secs(30)
        )
        .await,
        None
    );
}
