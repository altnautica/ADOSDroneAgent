//! `GET|POST /api/compute/workstation-credential` — the credentials
//! workstations issued this node.
//!
//! A node's own pairing key means nothing to a workstation, so the ground
//! station, which holds the owner key of both, asks the workstation to issue
//! this node a credential scoped to the lanes it uses and installs it here. The
//! lanes (the offload session, the Atlas forwarder, a ground station's Atlas
//! relay, the plugin offload lane) then present it to the workstation that
//! issued it, picked by that workstation's advertised node id.
//!
//! - `POST` `{ workstation_node_id, credential, lanes }` installs one,
//!   replacing an earlier credential from the same workstation.
//! - `GET` lists what is installed, never the secret.
//!
//! The store is owner-only (0600) at
//! [`ados_protocol::node_credential::WORKSTATION_CREDENTIALS_PATH`]. Served with
//! the front's native auth posture (key-gated when the agent is paired).

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use ados_protocol::node_credential::{InstalledCredential, NodeLane, WorkstationCredentials};

use crate::routes::detail;

/// A node id, not free text.
const MAX_NODE_ID_LEN: usize = 128;
/// Far above an issued token's length; bounds what is written to disk.
const MAX_CREDENTIAL_LEN: usize = 256;

#[derive(Debug, Deserialize)]
pub struct InstallRequest {
    workstation_node_id: String,
    credential: String,
    lanes: Vec<NodeLane>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `GET /api/compute/workstation-credential`.
pub async fn get_workstation_credentials() -> Response {
    let path = WorkstationCredentials::default_path();
    match tokio::task::spawn_blocking(move || list_at(&path)).await {
        Ok(resp) => resp,
        Err(e) => detail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `POST /api/compute/workstation-credential`.
pub async fn install_workstation_credential(Json(req): Json<InstallRequest>) -> Response {
    let path = WorkstationCredentials::default_path();
    match tokio::task::spawn_blocking(move || install_at(&path, req, now_ms())).await {
        Ok(resp) => resp,
        Err(e) => detail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

fn list_at(path: &Path) -> Response {
    match WorkstationCredentials::load(path) {
        Ok(store) => Json(json!({
            "workstations": store
                .workstations
                .iter()
                .map(|c| json!({
                    "workstation_node_id": c.workstation_node_id,
                    "lanes": c.lanes,
                    "installed_at_ms": c.installed_at_ms,
                }))
                .collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => detail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("the workstation credential store is unreadable: {e}"),
        ),
    }
}

fn install_at(path: &Path, req: InstallRequest, now_ms: i64) -> Response {
    let node_id = req.workstation_node_id.trim();
    if node_id.is_empty() || node_id.len() > MAX_NODE_ID_LEN {
        return detail(
            StatusCode::BAD_REQUEST,
            "workstation_node_id must be 1-128 characters",
        );
    }
    let credential = req.credential.trim();
    let header_safe = credential.bytes().all(|b| b.is_ascii_graphic());
    if credential.is_empty() || credential.len() > MAX_CREDENTIAL_LEN || !header_safe {
        return detail(StatusCode::BAD_REQUEST, "credential is not a valid token");
    }
    if req.lanes.is_empty() {
        return detail(StatusCode::BAD_REQUEST, "at least one lane is required");
    }
    // An unreadable store is replaced rather than refusing the install: every
    // credential it held is equally unusable, and the owner is re-provisioning.
    let mut store = WorkstationCredentials::load_or_empty(path);
    store.upsert(InstalledCredential {
        workstation_node_id: node_id.to_string(),
        credential: credential.to_string(),
        lanes: req.lanes.clone(),
        installed_at_ms: now_ms,
    });
    if let Err(e) = store.save(path) {
        tracing::error!(error = %e, "workstation credential install failed");
        return detail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not store the credential: {e}"),
        );
    }
    tracing::info!(workstation = %node_id, "workstation credential installed");
    Json(json!({
        "installed": true,
        "workstation_node_id": node_id,
        "lanes": req.lanes,
        "installed_at_ms": now_ms,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(node: &str, cred: &str) -> InstallRequest {
        InstallRequest {
            workstation_node_id: node.into(),
            credential: cred.into(),
            lanes: NodeLane::ALL.to_vec(),
        }
    }

    async fn body(resp: Response) -> (StatusCode, serde_json::Value) {
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn install_stores_the_credential_and_the_listing_never_returns_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let (st, _) = body(install_at(&path, req("ws-a", "nc1.ID.SECRET"), 5)).await;
        assert_eq!(st, StatusCode::OK);
        // The lanes read it back for the issuing workstation only.
        let store = WorkstationCredentials::load(&path).unwrap();
        assert_eq!(
            store.for_node(Some("ws-a")).unwrap().credential,
            "nc1.ID.SECRET"
        );
        assert_eq!(store.for_node(Some("ws-b")), None);

        let (st, listed) = body(list_at(&path)).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(listed["workstations"][0]["workstation_node_id"], "ws-a");
        assert!(!listed.to_string().contains("SECRET"));
    }

    #[tokio::test]
    async fn a_reinstall_from_the_same_workstation_replaces_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        install_at(&path, req("ws-a", "old"), 1);
        install_at(&path, req("ws-a", "new"), 2);
        let store = WorkstationCredentials::load(&path).unwrap();
        assert_eq!(store.workstations.len(), 1);
        assert_eq!(store.workstations[0].credential, "new");
    }

    #[tokio::test]
    async fn malformed_installs_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        for bad in [
            req("", "nc1.a.b"),
            req("ws", ""),
            req("ws", "has space"),
            req("ws", "line\nbreak"),
            InstallRequest {
                lanes: vec![],
                ..req("ws", "nc1.a.b")
            },
        ] {
            assert_eq!(install_at(&path, bad, 1).status(), StatusCode::BAD_REQUEST);
        }
        assert!(!path.exists());
    }
}
