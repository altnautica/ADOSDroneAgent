//! Pre-flight readiness items and the structured detail behind them.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Block,
    Warn,
}

/// One pre-flight readiness item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Readiness {
    pub id: String,
    pub ok: bool,
    pub severity: Severity,
    pub msg: String,
    /// The parameters behind the item, one row each: what the FC has and
    /// what the item wants. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<ParamDiff>,
    /// Where a surface sends the operator to fix the item. Omitted when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<ReadinessFix>,
}

/// One parameter of a readiness item: the FC's value (`None` when not read
/// or absent) and the wanted value as text (`"1"`, `"5 or 7"`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParamDiff {
    pub name: String,
    pub have: Option<f64>,
    pub want: String,
}

/// The action that fixes a readiness item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReadinessFix {
    /// Run the setup engine's `ados_profile` step.
    ApplyProfile,
    /// Open Aircraft setup at this step.
    SetupStep { step: String },
    /// Open this settings section.
    Settings { section: String },
    /// Open the parameter editor filtered to these names.
    Parameters { names: Vec<String> },
}
