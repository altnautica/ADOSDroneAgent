//! `POST /api/v1/ground-station/factory-reset?confirm=<token>` — return a ground
//! station to its first-boot posture.
//!
//! Refused unless the caller is on the box itself: the edge classifies every
//! request ([`CallerClass`]) and only [`CallerClass::OnBox`] — the operator
//! socket, or loopback with no forwarding header — may wipe the node. A
//! remote operator holding the pairing key cannot, which is the posture the
//! route documents: handing a unit on means someone has it in hand. The
//! `confirm` token is the destructive-action confirmation on top of that: the
//! installed fleet key's fingerprint, or `factory-reset-unpaired` on a station
//! that holds none.
//!
//! The order matters:
//!
//! 1. The supervisor moves the node to the `direct` mesh role, which stops the
//!    mesh services, so nothing is reading `/etc/ados/mesh` while it is wiped. A
//!    failed transition aborts the reset with nothing wiped.
//! 2. The data-plane service unpairs (wipes the fleet keys, restarts the
//!    receive unit). When that service is not running the key directory is
//!    still wiped below.
//! 3. Every standing credential, then identity and configuration, is deleted:
//!    the same set `scripts/factory-reset.sh` removes. `profile.conf` stays: it
//!    records what the hardware is, not who owns it.
//! 4. The mesh identity, revocations, role sentinel and gateway pin go.
//!
//! The configuration file is deleted outright, so the auto-pair and hotspot
//! settings fall back to their defaults (auto-pair armed, a freshly generated
//! AP passphrase) with nothing further written.

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use ados_protocol::pairing_posture::CallerClass;

use crate::routes::gs_cmd::groundlink_cmd_roundtrip;
use crate::state::{AppState, PairingPaths};
use crate::wfb_pair_state::read_public_fingerprint;

/// The confirmation token on a station that holds no fleet key.
const UNPAIRED_CONFIRM_TOKEN: &str = "factory-reset-unpaired";

/// The 64-byte wfb-ng key file size.
const WFB_KEY_FILE_BYTES: u64 = 64;

/// How long the role transition may take: it stops up to four units.
const ROLE_TRANSITION_TIMEOUT: Duration = Duration::from_secs(60);

fn nested_detail(status: StatusCode, error: Value) -> Response {
    (status, Json(json!({ "detail": { "error": error } }))).into_response()
}

fn is_ground_station() -> bool {
    let cfg = crate::config::PairingConfig::load();
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile == "ground-station"
}

/// The `?confirm=` query.
#[derive(Debug, Deserialize)]
pub struct FactoryResetQuery {
    pub confirm: String,
}

/// Everything a reset removes, resolved once so a test can point it at a
/// tempdir.
#[derive(Debug, Clone)]
pub(crate) struct ResetTargets {
    /// Credentials first, then identity and configuration: an interrupted run
    /// has already destroyed what grants access.
    files: Vec<PathBuf>,
    dirs: Vec<PathBuf>,
    /// Mesh identity and state, wiped after the role transition.
    mesh_files: Vec<PathBuf>,
    mesh_id: PathBuf,
    mesh_psk: PathBuf,
    rx_key: PathBuf,
}

impl ResetTargets {
    /// The production set, derived from the state's path seams. The etc dir is
    /// the config file's parent.
    fn from_paths(paths: &PairingPaths) -> Self {
        let etc = paths
            .config
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/etc/ados"));
        let mesh = paths
            .mesh_role
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| etc.join("mesh"));
        Self::build(
            &etc,
            &paths.pairing_json,
            &paths.config,
            &paths.wfb_key_dir,
            &mesh,
            &paths.mesh_role,
            Path::new("/etc"),
            Path::new("/var/lib/ados"),
            Path::new("/var/log/ados"),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        etc: &Path,
        pairing_json: &Path,
        config: &Path,
        wfb_key_dir: &Path,
        mesh: &Path,
        mesh_role: &Path,
        system_etc: &Path,
        var_lib: &Path,
        log_dir: &Path,
    ) -> Self {
        Self {
            files: vec![
                pairing_json.to_path_buf(),
                etc.join("dashboard-pin.json"),
                etc.join("mcp-token.json"),
                etc.join("ap-passphrase"),
                // The bind's shared radio keys, which also seed the swarm-bus
                // fleet key and the presence-beacon key.
                system_etc.join("drone.key"),
                system_etc.join("gs.key"),
                var_lib.join("setup-complete"),
                etc.join("device-id"),
                config.to_path_buf(),
            ],
            dirs: vec![
                etc.join("secrets"),
                wfb_key_dir.to_path_buf(),
                etc.join("certs"),
                log_dir.to_path_buf(),
            ],
            mesh_files: vec![
                mesh.join("id"),
                mesh.join("psk.key"),
                mesh.join("receiver.json"),
                mesh.join("revocations.json"),
                mesh_role.to_path_buf(),
                mesh.join("gateway.json"),
            ],
            mesh_id: mesh.join("id"),
            mesh_psk: mesh.join("psk.key"),
            rx_key: wfb_key_dir.join("rx.key"),
        }
    }

    /// The token that confirms a reset: the installed fleet key's fingerprint,
    /// or the stock token when no readable key is installed.
    fn expected_confirm(&self) -> String {
        let installed = std::fs::metadata(&self.rx_key)
            .map(|m| m.is_file() && m.len() == WFB_KEY_FILE_BYTES)
            .unwrap_or(false);
        installed
            .then(|| read_public_fingerprint(&self.rx_key))
            .flatten()
            .unwrap_or_else(|| UNPAIRED_CONFIRM_TOKEN.to_owned())
    }

    fn has_mesh_identity(&self) -> bool {
        self.mesh_id.is_file() && self.mesh_psk.is_file()
    }
}

/// Remove a file; absent is success. Failures are collected, not fatal: a
/// reset that stops at the first stubborn file leaves more behind.
fn remove_file(path: &Path, failures: &mut Vec<String>) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "factory_reset_delete_failed");
            failures.push(path.display().to_string());
        }
    }
}

fn remove_dir(path: &Path, failures: &mut Vec<String>) {
    match std::fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "factory_reset_delete_failed");
            failures.push(path.display().to_string());
        }
    }
}

/// Wipe the credential, identity and configuration set. Returns the paths that
/// could not be removed.
fn wipe_credentials(targets: &ResetTargets) -> Vec<String> {
    let mut failures = Vec::new();
    for f in &targets.files {
        remove_file(f, &mut failures);
    }
    for d in &targets.dirs {
        remove_dir(d, &mut failures);
    }
    failures
}

/// Wipe the mesh identity and state. Returns the paths that could not be
/// removed.
fn wipe_mesh(targets: &ResetTargets) -> Vec<String> {
    let mut failures = Vec::new();
    for f in &targets.mesh_files {
        remove_file(f, &mut failures);
    }
    failures
}

/// `POST .../factory-reset` → `{reset, timestamp, auto_pair_enabled, mesh}`.
pub async fn post_factory_reset(
    State(state): State<AppState>,
    caller: Option<Extension<CallerClass>>,
    Query(query): Query<FactoryResetQuery>,
) -> Response {
    if !is_ground_station() {
        return nested_detail(StatusCode::NOT_FOUND, json!({"code": "E_PROFILE_MISMATCH"}));
    }
    let caller = caller.map_or(CallerClass::Remote, |Extension(c)| c);
    let targets = ResetTargets::from_paths(&state.pairing_paths);
    if let Err(refusal) = admit(caller, &query.confirm, &targets) {
        return refusal;
    }

    let had_identity = targets.has_mesh_identity();

    // 1. Leave any mesh role before anything under /etc/ados/mesh is touched.
    let request = json!({"op": "set_role", "role": "direct", "reason": "factory_reset"});
    let sock = crate::routes::gs_mesh_write::supervisor_sock();
    match crate::ipc::cmd::roundtrip_object(&sock, &request, ROLE_TRANSITION_TIMEOUT).await {
        Ok(reply) if reply.get("ok") != Some(&Value::Bool(false)) => {}
        outcome => {
            let reason = match outcome {
                Ok(reply) => reply
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("the supervisor refused the role change")
                    .to_owned(),
                Err(_) => "the supervisor control socket did not answer".to_owned(),
            };
            return nested_detail(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({
                    "code": "E_FACTORY_RESET_ROLE_FAILED",
                    "message": format!(
                        "Could not move to the direct role before the wipe: {reason}. \
                         Nothing was wiped. Stop the mesh services and retry."
                    ),
                }),
            );
        }
    }

    // 2. Unpair through the data-plane service, which also restarts the receive
    //    unit. With the service down the key directory is wiped below anyway.
    if let Some(reply) = groundlink_cmd_roundtrip(&json!({"op": "unpair"})).await {
        if reply.get("ok") == Some(&Value::Bool(false)) {
            let message = reply
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the ground-station service refused to unpair");
            return nested_detail(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"code": "E_FACTORY_RESET_FAILED", "message": message}),
            );
        }
    }

    // 3 + 4. Credentials, identity, configuration; then the mesh state. The
    // pairing document is removed under the pairing writer lock so a claim in
    // flight cannot rewrite it afterwards.
    let (failures, mesh_failures) = {
        let _writers = crate::pairing_store::lock_writers(&state.pairing_paths.pairing_json).await;
        let t = targets.clone();
        match tokio::task::spawn_blocking(move || (wipe_credentials(&t), wipe_mesh(&t))).await {
            Ok(result) => result,
            Err(e) => {
                return nested_detail(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"code": "E_FACTORY_RESET_FAILED", "message": e.to_string()}),
                )
            }
        }
    };

    let timestamp = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    tracing::warn!(timestamp = %timestamp, "factory_reset_performed");

    let mut mesh = Map::new();
    mesh.insert(
        "cleared_mesh".into(),
        json!(had_identity && mesh_failures.is_empty()),
    );
    mesh.insert("role".into(), json!("direct"));
    if !mesh_failures.is_empty() {
        mesh.insert(
            "error".into(),
            json!(format!("could not remove {}", mesh_failures.join(", "))),
        );
    }
    let mut body = json!({
        "reset": failures.is_empty(),
        "timestamp": timestamp,
        "auto_pair_enabled": true,
        "mesh": Value::Object(mesh),
    });
    if !failures.is_empty() {
        body["not_removed"] = json!(failures);
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response();
    }
    Json(body).into_response()
}

/// The two gates in front of the wipe: the caller is on the box, and the
/// confirmation token matches.
#[allow(clippy::result_large_err)]
fn admit(caller: CallerClass, confirm: &str, targets: &ResetTargets) -> Result<(), Response> {
    if caller != CallerClass::OnBox {
        return Err(nested_detail(
            StatusCode::FORBIDDEN,
            json!({
                "code": "E_NOT_ON_BOX",
                "message": "A factory reset can only be started on the device itself.",
            }),
        ));
    }
    if !ados_protocol::pairing_posture::constant_time_eq(
        confirm.as_bytes(),
        targets.expected_confirm().as_bytes(),
    ) {
        return Err(nested_detail(
            StatusCode::BAD_REQUEST,
            json!({"code": "E_CONFIRM_MISMATCH"}),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(root: &Path) -> ResetTargets {
        let etc = root.join("etc/ados");
        let mesh = etc.join("mesh");
        std::fs::create_dir_all(&mesh).unwrap();
        ResetTargets::build(
            &etc,
            &etc.join("pairing.json"),
            &etc.join("config.yaml"),
            &etc.join("wfb"),
            &mesh,
            &mesh.join("role"),
            &root.join("etc"),
            &root.join("var/lib/ados"),
            &root.join("var/log/ados"),
        )
    }

    #[test]
    fn a_remote_or_relayed_caller_cannot_reset_even_with_the_right_token() {
        let tmp = tempfile::tempdir().unwrap();
        let t = targets(tmp.path());
        for caller in [
            CallerClass::Remote,
            CallerClass::OperatorLan,
            CallerClass::Lifeline,
        ] {
            let refused = admit(caller, UNPAIRED_CONFIRM_TOKEN, &t).unwrap_err();
            assert_eq!(refused.status(), StatusCode::FORBIDDEN, "{caller:?}");
        }
        assert!(admit(CallerClass::OnBox, UNPAIRED_CONFIRM_TOKEN, &t).is_ok());
    }

    #[test]
    fn the_stock_token_does_not_confirm_a_paired_station() {
        let tmp = tempfile::tempdir().unwrap();
        let t = targets(tmp.path());
        std::fs::create_dir_all(t.rx_key.parent().unwrap()).unwrap();
        std::fs::write(&t.rx_key, [7u8; 64]).unwrap();
        let expected = t.expected_confirm();
        assert_ne!(expected, UNPAIRED_CONFIRM_TOKEN);
        let refused = admit(CallerClass::OnBox, UNPAIRED_CONFIRM_TOKEN, &t).unwrap_err();
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
        assert!(admit(CallerClass::OnBox, &expected, &t).is_ok());
        assert_eq!(
            admit(CallerClass::OnBox, "wrong", &t).unwrap_err().status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn the_wipe_removes_every_credential_and_keeps_the_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let t = targets(tmp.path());
        let etc = tmp.path().join("etc/ados");
        for f in t.files.iter().chain(t.mesh_files.iter()) {
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, "x").unwrap();
        }
        for d in &t.dirs {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("inner"), "x").unwrap();
        }
        std::fs::write(etc.join("profile.conf"), "profile: ground_station\n").unwrap();

        assert!(wipe_credentials(&t).is_empty());
        assert!(wipe_mesh(&t).is_empty());
        for p in t
            .files
            .iter()
            .chain(t.dirs.iter())
            .chain(t.mesh_files.iter())
        {
            assert!(!p.exists(), "{} survived the reset", p.display());
        }
        assert!(etc.join("profile.conf").is_file());
        // A second run over the wiped tree is clean, not an error.
        assert!(wipe_credentials(&t).is_empty());
    }
}
