//! The engine stamps the multi-object tracker's ids on every detection and
//! keeps the designated subject's lock on the id `designate` returned.

use super::*;
use crate::backend::MockBackend;
use ados_protocol::framebus::{BoundingBox, LockState, TrackState};

fn person(x: f32, conf: f32) -> Detection {
    Detection {
        bbox: Some(BoundingBox {
            x,
            y: 100.0,
            width: 40.0,
            height: 100.0,
        }),
        class_label: "person".into(),
        confidence: conf,
        track_id: None,
        assoc_confidence: None,
        lock_state: None,
        attributes: None,
        mask: None,
        keypoints: None,
        depth: None,
        world_pos: None,
        track_state: None,
        appearance_id: None,
    }
}

async fn step(e: &VisionEngine, dets: Vec<Detection>) -> Vec<Detection> {
    e.apply_tracker("cam", dets, &[], 0, 0, FrameFormat::Rgb24)
        .await
}

fn ids(dets: &[Detection]) -> Vec<Option<u64>> {
    dets.iter().map(|d| d.track_id).collect()
}

#[tokio::test]
async fn every_person_gets_an_id_and_the_designated_one_keeps_the_returned_id() {
    let e = VisionEngine::with_tracker(Box::new(MockBackend), 4, true, TrackerConfig::default());
    let pair = || vec![person(100.0, 0.9), person(400.0, 0.85)];
    let mut out = Vec::new();
    for _ in 0..3 {
        out = step(&e, pair()).await;
    }
    let before = ids(&out);
    assert!(before.iter().all(Option::is_some), "{out:?}");
    assert_ne!(before[0], before[1]);
    assert!(out
        .iter()
        .all(|d| d.track_state == Some(TrackState::Confirmed)));
    // The auto-pick (the more confident one) is tracked but never locked.
    assert!(out.iter().all(|d| d.lock_state.is_none()));

    let id = e.designate("cam", &pair()[1]).await.unwrap();
    assert!(
        !before.contains(&Some(id)),
        "a designation mints a fresh id"
    );
    assert_eq!(e.current_track("cam").await, Some(id));
    for _ in 0..2 {
        out = step(&e, pair()).await;
    }
    assert_eq!(out[1].track_id, Some(id));
    assert_eq!(out[1].lock_state, Some(LockState::Locked));
    assert_eq!(out[0].track_id, before[0], "the other person keeps its id");
    assert!(
        out[1].appearance_id.is_some(),
        "the subject carries its appearance id"
    );
    assert_eq!(out[0].appearance_id, None);

    // The subject is missed: the lock's predicted box alone carries its id.
    out = step(&e, vec![person(100.0, 0.9)]).await;
    let held: Vec<_> = out.iter().filter(|d| d.track_id == Some(id)).collect();
    assert_eq!(held.len(), 1, "{out:?}");
    assert_eq!(held[0].track_state, Some(TrackState::Lost));
    assert_eq!(held[0].lock_state, Some(LockState::Uncertain));
    assert_eq!(out[0].track_id, before[0]);
}

#[tokio::test]
async fn a_lone_low_score_subject_is_tracked_once_designated() {
    // A detector reporting the subject at a low score never starts a track on
    // its own, but the operator's designation does, under the returned id.
    let e = VisionEngine::with_tracker(Box::new(MockBackend), 4, true, TrackerConfig::default());
    let subject = person(200.0, 0.4);
    let out = step(&e, vec![subject.clone()]).await;
    assert_eq!(out[0].track_id, None);
    let id = e.designate("cam", &subject).await.unwrap();
    for _ in 0..3 {
        let out = step(&e, vec![subject.clone()]).await;
        assert_eq!(out[0].track_id, Some(id), "{out:?}");
    }
    e.reset_track("cam").await;
    assert_eq!(e.current_track("cam").await, None);
}
