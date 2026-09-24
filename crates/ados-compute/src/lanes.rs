//! The lanes other nodes use on this node's listener, each behind its gate.
//!
//! Beside the owner's job API the node serves four lanes a drone reaches: the
//! reconstruction artifacts, the offloaded-detection return stream, and (on an
//! Atlas node) the capture-event ingest and the world-model descriptor stream.
//! Each is mounted here and nowhere else, wrapped in
//! [`crate::auth::require_lane`] for its own [`NodeLane`], so a lane cannot be
//! mounted without its gate: a paired node serves it only to the owner, the
//! on-box operator, or a node holding a credential issued for that lane.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use tokio::sync::mpsc::Sender;

use ados_atlas_transport::{atlas_event_router, world_ws_router, AtlasEvent, WorldBroadcaster};
use ados_protocol::node_credential::NodeLane;

use crate::artifacts::artifact_router;
use crate::auth::{require_lane, ComputeAuth};
use crate::offload_ws::{offload_ws_router, DetectionBroadcaster};

/// The Atlas lanes, mounted only while Atlas is enabled on this node.
pub struct AtlasLanes {
    /// Where decoded capture events go (the receiver loop drains it).
    pub events: Sender<AtlasEvent>,
    /// The world-model descriptor fan-out.
    pub world: Arc<WorldBroadcaster>,
}

/// What the lane routers serve.
pub struct LaneRoutes {
    /// The artifact work root (path-jailed by the artifact router).
    pub work_root: PathBuf,
    /// The offload detection fan-out.
    pub offload: Arc<DetectionBroadcaster>,
    /// The Atlas lanes, or `None` on a node without Atlas.
    pub atlas: Option<AtlasLanes>,
}

/// Every lane router, each behind the gate for its lane.
pub fn lane_router(auth: Arc<ComputeAuth>, routes: LaneRoutes) -> Router {
    let gate = |router: Router, lane: NodeLane| {
        router.route_layer(axum::middleware::from_fn_with_state(
            (auth.clone(), lane),
            require_lane,
        ))
    };
    let mut router = gate(artifact_router(routes.work_root), NodeLane::Artifacts).merge(gate(
        offload_ws_router(routes.offload),
        NodeLane::OffloadStream,
    ));
    if let Some(atlas) = routes.atlas {
        router = router
            .merge(gate(
                atlas_event_router(atlas.events),
                NodeLane::AtlasIngest,
            ))
            .merge(gate(world_ws_router(atlas.world), NodeLane::AtlasWorld));
    }
    router
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_credentials::NodeCredentialStore;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tokio::sync::mpsc::{channel, Receiver};
    use tower::ServiceExt;

    const OWNER: &str = "ados_secret";
    const OFFBOX: &str = "192.168.1.50:55000";

    struct Fixture {
        router: Router,
        auth: Arc<ComputeAuth>,
        events: Receiver<AtlasEvent>,
        _dir: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let pairing = dir.path().join("pairing.json");
        std::fs::write(
            &pairing,
            format!(r#"{{"paired": true, "api_key": "{OWNER}"}}"#),
        )
        .unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir_all(work.join("job-1")).unwrap();
        std::fs::write(work.join("job-1/cloud.ply"), b"ply").unwrap();
        let auth = Arc::new(ComputeAuth::new(
            pairing,
            NodeCredentialStore::open(dir.path().join("creds.json"), "ws-node"),
        ));
        let (tx, rx) = channel(8);
        let router = lane_router(
            auth.clone(),
            LaneRoutes {
                work_root: work,
                offload: Arc::new(DetectionBroadcaster::new(8)),
                atlas: Some(AtlasLanes {
                    events: tx,
                    world: Arc::new(WorldBroadcaster::new(8)),
                }),
            },
        );
        Fixture {
            router,
            auth,
            events: rx,
            _dir: dir,
        }
    }

    async fn status(
        router: &Router,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> StatusCode {
        let mut builder = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        let mut req = builder.body(Body::from(body)).unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo::<std::net::SocketAddr>(
                OFFBOX.parse().unwrap(),
            ));
        router.clone().oneshot(req).await.unwrap().status()
    }

    fn event_body() -> Vec<u8> {
        AtlasEvent::new("atlas.keyframe", Some("drone-1".into()), vec![1, 2, 3])
            .encode()
            .unwrap()
    }

    #[tokio::test]
    async fn every_lane_refuses_an_offbox_caller_without_a_credential() {
        let f = fixture();
        assert_eq!(
            status(&f.router, "GET", "/artifacts/job-1/cloud.ply", &[], vec![]).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(&f.router, "GET", "/ws/offload/s1", &[], vec![]).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(&f.router, "POST", "/api/atlas/event", &[], event_body()).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(&f.router, "GET", "/api/atlas/health", &[], vec![]).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(&f.router, "GET", "/ws/atlas/drone-1", &[], vec![]).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn a_drone_credential_reaches_the_lanes_it_was_issued_for() {
        let mut f = fixture();
        let m = f
            .auth
            .credentials
            .mint("drone-1", &[NodeLane::AtlasIngest], OWNER, 1)
            .unwrap();
        let cred = [("x-ados-node-credential", m.credential.as_str())];
        assert_eq!(
            status(&f.router, "POST", "/api/atlas/event", &cred, event_body()).await,
            StatusCode::ACCEPTED
        );
        assert_eq!(f.events.recv().await.unwrap().topic, "atlas.keyframe");
        // Not issued for artifacts.
        assert_eq!(
            status(
                &f.router,
                "GET",
                "/artifacts/job-1/cloud.ply",
                &cred,
                vec![]
            )
            .await,
            StatusCode::UNAUTHORIZED
        );
        // The owner reaches the artifact.
        assert_eq!(
            status(
                &f.router,
                "GET",
                "/artifacts/job-1/cloud.ply",
                &[("x-ados-key", OWNER)],
                vec![]
            )
            .await,
            StatusCode::OK
        );
    }
}
