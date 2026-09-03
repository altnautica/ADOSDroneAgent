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
//! that is precisely what moving the normalisation in-memory bought.

use crate::ctx::Ctx;
use crate::exec;
use crate::graph::{Step, StepKind, StepOutcome};
use crate::steps::venv_agent::venv_python;

/// The migration entrypoint inside the agent package. Invoked as a module so
/// the step does not depend on the `ados` console script being on PATH yet
/// (`start` has not run at this point in the chain).
const MIGRATE_ARGS: &[&str] = &["-m", "ados.cli.main", "config", "migrate", "--json"];

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
    fn run(&self, _ctx: &mut Ctx) -> StepOutcome {
        let res = exec::run(&venv_python(), MIGRATE_ARGS);
        classify(res.spawned, res.code, &res.stdout)
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
}
