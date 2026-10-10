//! Config migration: bring `/etc/ados/config.yaml` to the current shape once,
//! here, instead of on every unit's startup path.
//!
//! This step exists because migration used to happen implicitly inside the
//! agent's `load_config()`. Four side-file migrators each serialised the whole
//! merged mapping back over `/etc/ados/config.yaml` with a tmp-write and an
//! `os.replace` — from a function every one of the node's units calls on its
//! own startup path, concurrently, with no lock, against the one file that
//! carries the WFB pairing key, the profile and the role. The native writers
//! serialise on `/run/ados/config.yaml.lock`; a lock only one participant takes
//! protects nobody.
//!
//! Migration is an install-time action, so it runs once per install and once
//! per upgrade, from a process nothing is waiting on, under that same lock.
//! The agent's read path now normalises legacy shapes in memory and never
//! writes.
//!
//! **Optional, deliberately.** A node whose config is already current, or whose
//! lock is held by a concurrent writer, is not a failed install: the same pass
//! runs again on the next upgrade, and an operator can run
//! `sudo ados config migrate` at any time. What would be wrong is aborting an
//! install over it — the agent runs correctly on an un-migrated config, because
//! that is precisely what moving the normalisation in-memory bought. A held
//! lock is waited on first ([`LOCK_WAIT_ATTEMPTS`] retries, [`LOCK_RETRY`]
//! apart), so a writer that holds it briefly never costs the migration.
//!
//! Before migrating, the current config is copied to `config.yaml.pre-<ver>`
//! and recorded as this run's config snapshot, so a failed install restores it
//! (see [`crate::rollback`]). After migrating, the profile's injector
//! arbitration policy is applied ([`arbitration`]): a drone has a single
//! injector and drops `mavlink.injector_arbitration: true`; a ground station
//! gains it when absent.

mod arbitration;

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::ctx::Ctx;
use crate::env::{self, CONFIG_YAML};
use crate::exec::{self, CmdResult};
use crate::graph::{Step, StepKind, StepOutcome};
use crate::rollback::snapshots::{record_snapshot, KIND_CONFIG, RUN_RECORD};
use crate::steps::venv_agent::venv_python;

/// The migration entrypoint inside the agent package. Invoked as a module so
/// the step does not depend on the `ados` console script being on PATH yet
/// (`start` has not run at this point in the chain).
const MIGRATE_ARGS: &[&str] = &["-m", "ados.cli.main", "config", "migrate", "--json"];

/// How many times a held config lock is retried before the step reports it.
pub const LOCK_WAIT_ATTEMPTS: u32 = 10;

/// The pause between lock retries.
pub const LOCK_RETRY: Duration = Duration::from_secs(1);

/// Whether a migration run was turned away by a held config lock: a non-zero
/// exit whose envelope says `"locked": false`.
pub fn lock_unavailable(res: &CmdResult) -> bool {
    if !res.spawned || res.code == Some(0) {
        return false;
    }
    let compact: String = res.stdout.split_whitespace().collect();
    compact.contains("\"locked\":false")
}

/// Run `attempt`, re-running it after `pause(LOCK_RETRY)` while the config
/// lock is held, at most [`LOCK_WAIT_ATTEMPTS`] more times. Returns the last
/// result.
pub fn run_with_lock_wait(
    mut attempt: impl FnMut() -> CmdResult,
    mut pause: impl FnMut(Duration),
) -> CmdResult {
    let mut res = attempt();
    for _ in 0..LOCK_WAIT_ATTEMPTS {
        if !lock_unavailable(&res) {
            break;
        }
        tracing::info!("config lock held by another writer; retrying the migration");
        pause(LOCK_RETRY);
        res = attempt();
    }
    res
}

/// The pre-migration copy of `config` for agent version `version`.
pub fn pre_migration_path(config: &Path, version: &str) -> PathBuf {
    let mut name = config
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".pre-{version}"));
    config.with_file_name(name)
}

/// Copy `config` to its pre-migration path and record it as this run's config
/// snapshot. `None` when there is no config to keep or the copy failed (the
/// migration still runs; only the restore point is missing, and it is logged).
fn snapshot_config(config: &Path, version: &str, record: &Path) -> Option<PathBuf> {
    if !config.is_file() {
        return None;
    }
    let copy = pre_migration_path(config, version);
    // `fs::copy` carries the source's permission bits: the config holds secrets.
    if let Err(e) = std::fs::copy(config, &copy) {
        tracing::warn!(error = %e, "pre-migration config copy failed");
        return None;
    }
    if let Err(e) = record_snapshot(record, KIND_CONFIG, &copy) {
        tracing::warn!(error = %e, "config snapshot not recorded");
    }
    Some(copy)
}

/// The config lock path for the canonical config: under the agent's run
/// directory (`ADOS_RUN_DIR`, default `/run/ados`), shared with every writer.
fn config_lock_path() -> PathBuf {
    std::env::var_os("ADOS_RUN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/ados"))
        .join("config.yaml.lock")
}

/// Run the migration and classify the outcome. Split out so a test drives the
/// classification without a venv.
///
/// `stdout` is the `--json` envelope; `code` is the process exit status.
pub fn classify(spawned: bool, code: Option<i32>, stdout: &str) -> StepOutcome {
    if !spawned {
        return StepOutcome::Skipped;
    }
    // The CLI exits non-zero only for a condition that is worth reporting and
    // retrying (an unavailable lock, an unparseable config, a failed write).
    if code != Some(0) {
        let detail = extract(stdout, "\"error\"").unwrap_or_else(|| "unknown".to_string());
        return StepOutcome::Failed(format!("config migration reported: {detail}"));
    }
    StepOutcome::Ok
}

/// Pull a JSON string value out of the CLI envelope without a serde_json
/// dependency in this crate. Returns `None` when the key is absent or null.
fn extract(stdout: &str, key: &str) -> Option<String> {
    let rest = stdout.split_once(key)?.1;
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    if rest.starts_with("null") {
        return None;
    }
    let body = rest.strip_prefix('"')?;
    let end = body.find('"')?;
    Some(body[..end].to_string())
}

/// The step.
pub struct ConfigMigrate;

impl Step for ConfigMigrate {
    fn id(&self) -> &str {
        "config_migrate"
    }
    fn requires(&self) -> &[&str] {
        // After the config exists and after the venv that hosts the migration
        // code. Before `systemd`/`start`, so no unit has yet loaded config.
        &["config_identity"]
    }
    fn checkpoint(&self) -> Option<&str> {
        // No checkpoint: the pass is idempotent, and skipping it on an upgrade
        // is the whole failure mode this step exists to prevent. A new
        // migration ships in a new version and has to reach an already
        // checkpointed node.
        None
    }
    fn kind(&self) -> StepKind {
        StepKind::Optional
    }
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        let config = Path::new(CONFIG_YAML);
        let version = env::installed_agent_version().unwrap_or_else(|| "unversioned".to_string());
        snapshot_config(config, &version, Path::new(RUN_RECORD));

        let res = run_with_lock_wait(
            || exec::run(&venv_python(), MIGRATE_ARGS),
            std::thread::sleep,
        );
        let outcome = classify(res.spawned, res.code, &res.stdout);
        if matches!(outcome, StepOutcome::Failed(_)) {
            return outcome;
        }
        if let Err(e) = arbitration::apply_policy(config, &config_lock_path(), &ctx.profile) {
            return StepOutcome::Failed(format!("injector arbitration policy: {e}"));
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_venv_skips_rather_than_failing() {
        // The step can be reached on a node whose venv step degraded. That is
        // already reported by `venv_agent`; reporting it twice as a config
        // failure would point the operator at the wrong thing.
        assert_eq!(classify(false, None, ""), StepOutcome::Skipped);
    }

    #[test]
    fn a_clean_run_is_ok() {
        let out = r#"{"path":"/etc/ados/config.yaml","locked":true,"changed":true,
            "applied":["api_from_scripting"],"error":null,"dry_run":false}"#;
        assert_eq!(classify(true, Some(0), out), StepOutcome::Ok);
    }

    #[test]
    fn an_already_current_config_is_ok() {
        let out = r#"{"path":"/etc/ados/config.yaml","locked":true,"changed":false,
            "applied":[],"error":null,"dry_run":false}"#;
        assert_eq!(classify(true, Some(0), out), StepOutcome::Ok);
    }

    #[test]
    fn a_held_lock_is_reported_with_its_reason() {
        // The reason matters: "lock unavailable" tells an operator to re-run,
        // where a bare failure tells them nothing.
        let out = r#"{"path":"/etc/ados/config.yaml","locked":false,"changed":false,
            "applied":[],"error":"config lock unavailable, migration skipped"}"#;
        match classify(true, Some(1), out) {
            StepOutcome::Failed(msg) => {
                assert!(msg.contains("config lock unavailable"), "{msg}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn a_nonzero_exit_with_no_parseable_error_still_reports() {
        match classify(true, Some(2), "traceback: boom") {
            StepOutcome::Failed(msg) => assert!(msg.contains("unknown"), "{msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn a_null_error_is_not_read_as_a_reason() {
        assert_eq!(extract(r#"{"error":null}"#, "\"error\""), None);
    }

    fn result(code: Option<i32>, stdout: &str) -> CmdResult {
        CmdResult {
            code,
            stdout: stdout.to_string(),
            stderr: String::new(),
            spawned: true,
        }
    }

    const HELD: &str = r#"{"locked": false, "changed": false,
        "error": "config lock unavailable, migration skipped"}"#;
    const DONE: &str = r#"{"locked": true, "changed": true, "error": null}"#;

    #[test]
    fn lock_held_2s_still_migrates() {
        // The lock is held for the first two attempts (about 2 s), then free.
        let mut calls = 0;
        let mut slept = Duration::ZERO;
        let res = run_with_lock_wait(
            || {
                calls += 1;
                if calls <= 2 {
                    result(Some(1), HELD)
                } else {
                    result(Some(0), DONE)
                }
            },
            |d| slept += d,
        );
        assert_eq!(calls, 3);
        assert_eq!(slept, Duration::from_secs(2));
        assert_eq!(
            classify(res.spawned, res.code, &res.stdout),
            StepOutcome::Ok
        );
    }

    #[test]
    fn a_lock_held_past_the_bound_is_reported() {
        let mut calls = 0;
        let res = run_with_lock_wait(
            || {
                calls += 1;
                result(Some(1), HELD)
            },
            |_| {},
        );
        assert_eq!(calls, 1 + LOCK_WAIT_ATTEMPTS);
        assert!(matches!(
            classify(res.spawned, res.code, &res.stdout),
            StepOutcome::Failed(_)
        ));
    }

    #[test]
    fn other_failures_are_not_retried() {
        let mut calls = 0;
        run_with_lock_wait(
            || {
                calls += 1;
                result(Some(2), "traceback: boom")
            },
            |_| {},
        );
        assert_eq!(calls, 1);
    }

    #[test]
    fn the_pre_migration_copy_sits_beside_the_config() {
        assert_eq!(
            pre_migration_path(Path::new("/etc/ados/config.yaml"), "0.103.0"),
            PathBuf::from("/etc/ados/config.yaml.pre-0.103.0")
        );
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        let record = dir.path().join("this-run");
        std::fs::write(&config, "agent:\n  profile: drone\n").unwrap();
        let copy = snapshot_config(&config, "0.103.0", &record).unwrap();
        assert_eq!(
            std::fs::read_to_string(&copy).unwrap(),
            "agent:\n  profile: drone\n"
        );
        assert_eq!(
            crate::rollback::snapshots::read_run_snapshots(&record).config,
            Some(copy)
        );
    }
}
