//! Every file the codegen writes from the agent's sources, in one list: the
//! Rust and Python registries inside the agent, the shared web types, and the
//! optional Mission Control mirrors (skipped when that repo is absent and
//! pinned by `gcs-mirror.sha256` either way).

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::contracts_emit::{emit_python_contracts, emit_rust_contracts, emit_ts_contracts};
use crate::schema_ts::{emit_ts_api_types, ApiSchema};
use crate::{
    emit_python, emit_python_dispatch, emit_python_wfb, emit_rust, emit_rust_dispatch,
    emit_rust_wfb, emit_ts_catalog, paths_emit, Catalog, Contracts, WfbTables,
};

pub(crate) struct Target {
    pub(crate) path: PathBuf,
    pub(crate) content: String,
    /// Skip (do not error in --check) when the parent repo is not checked out.
    pub(crate) optional: bool,
}

/// The committed API schemas, relative to the agent repo, with the exported
/// name of each root type.
const API_SCHEMAS: [(&str, &str); 3] = [
    ("schemas/api/status-full.schema.json", "StatusFull"),
    ("schemas/api/heartbeat.schema.json", "HeartbeatPayload"),
    ("schemas/api/setup-status.schema.json", "SetupStatus"),
];

fn read_schema(agent_repo: &Path, rel: &str) -> Value {
    let path = agent_repo.join(rel);
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// The agent API types (every schema) and the heartbeat-only module the
/// Convex backend imports (it cannot reach the app's `src/`).
fn api_type_modules(agent_repo: &Path) -> (String, String) {
    let schemas: Vec<(&str, &str, Value)> = API_SCHEMAS
        .iter()
        .map(|(rel, root)| (*rel, *root, read_schema(agent_repo, rel)))
        .collect();
    let all: Vec<ApiSchema<'_>> = schemas
        .iter()
        .map(|(file, root, schema)| ApiSchema { file, root, schema })
        .collect();
    let all_ts = emit_ts_api_types(&all, "schemas/api/*.schema.json");
    let heartbeat: Vec<ApiSchema<'_>> = all
        .iter()
        .filter(|s| s.root == "HeartbeatPayload")
        .copied()
        .collect();
    let heartbeat_ts = emit_ts_api_types(&heartbeat, "schemas/api/heartbeat.schema.json");
    (all_ts, heartbeat_ts)
}

/// The agent-internal targets plus the GCS mirrors.
pub(crate) fn gcs_and_local_targets(
    workspace: &Path,
    agent_repo: &Path,
    monorepo: &Path,
    cat: &Catalog,
    wfb: &WfbTables,
    contracts: &Contracts,
) -> Vec<Target> {
    let gcs = monorepo.join("ADOSMissionControl");
    let gcs_plugins = gcs.join("src/lib/plugins");
    let (api_ts, heartbeat_ts) = api_type_modules(agent_repo);
    let local = |path: PathBuf, content: String| Target {
        path,
        content,
        optional: false,
    };
    let mirror = |path: PathBuf, content: String| Target {
        path,
        content,
        optional: true,
    };
    vec![
        local(
            workspace.join("ados-protocol/src/capabilities.rs"),
            emit_rust(cat),
        ),
        local(
            agent_repo.join("src/ados/plugins/_capabilities_generated.py"),
            emit_python(cat),
        ),
        local(
            workspace.join("ados-protocol/src/dispatch.rs"),
            emit_rust_dispatch(&cat.method),
        ),
        local(
            agent_repo.join("src/ados/plugins/_dispatch_generated.py"),
            emit_python_dispatch(&cat.method),
        ),
        local(
            workspace.join("ados-protocol/src/wfb_tables.rs"),
            emit_rust_wfb(wfb),
        ),
        local(
            agent_repo.join("src/ados/services/wfb/_wfb_tables_generated.py"),
            emit_python_wfb(wfb),
        ),
        local(
            workspace.join("ados-protocol/src/contracts.rs"),
            emit_rust_contracts(contracts),
        ),
        local(
            agent_repo.join("src/ados/core/_contracts_generated.py"),
            emit_python_contracts(contracts),
        ),
        local(
            workspace.join("ados-protocol/src/paths.rs"),
            paths_emit::emit_rust(&contracts.sidecar, &contracts.socket),
        ),
        local(
            agent_repo.join("src/ados/core/_paths_generated.py"),
            paths_emit::emit_python(&contracts.sidecar, &contracts.socket),
        ),
        local(
            agent_repo.join("web-shared/src/agent-api.generated.ts"),
            api_ts.clone(),
        ),
        mirror(
            gcs_plugins.join("contracts.generated.ts"),
            emit_ts_contracts(contracts),
        ),
        mirror(
            gcs_plugins.join("agent-capabilities.generated.ts"),
            emit_ts_catalog(&cat.agent, "AGENT_CAPABILITY_CATALOG", None),
        ),
        mirror(
            gcs_plugins.join("gcs-capabilities.generated.ts"),
            emit_ts_catalog(&cat.gcs, "GCS_CAPABILITY_CATALOG", Some("GCS_CAPABILITIES")),
        ),
        mirror(gcs.join("src/lib/agent/agent-api.generated.ts"), api_ts),
        mirror(
            gcs.join("convex/lib/agentHeartbeat.generated.ts"),
            heartbeat_ts,
        ),
    ]
}
