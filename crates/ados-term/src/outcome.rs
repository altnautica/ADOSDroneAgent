//! The per-step result the progress renderers fold into checklist groups.

/// The result of running (or not running) a single step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    /// The step did its work successfully.
    Ok,
    /// The step had nothing to do (e.g. already done on an earlier run, or not
    /// applicable). Treated as success for dependents.
    Skipped,
    /// The step failed, with a human-readable reason.
    Failed(String),
}
