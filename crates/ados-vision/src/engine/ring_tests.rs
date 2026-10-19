//! Frame ring publish, growth and byte-budget tests.

use super::tests::engine;
use super::*;
use crate::backend::MockBackend;
use crate::ring::{fit_slot_count, FrameTiming};

#[tokio::test]
async fn publishes_frame_and_descriptor() {
    let e = engine();
    let mut rx = e.subscribe_frames();
    let data = vec![0u8; FrameFormat::Rgb24.frame_bytes(8, 8)];
    let desc = e
        .publish_frame(
            "uvc-0",
            1,
            FrameTiming::host(100),
            8,
            8,
            FrameFormat::Rgb24,
            &data,
        )
        .await
        .unwrap();
    assert_eq!(desc.camera_id, "uvc-0");
    assert_eq!(desc.seq, 1);
    let got = rx.try_recv().unwrap();
    assert_eq!(got, desc);
}

#[tokio::test]
async fn ring_grows_for_a_larger_frame() {
    let e = engine();
    let small = vec![0u8; FrameFormat::Rgb24.frame_bytes(8, 8)];
    e.publish_frame(
        "ring-grow",
        1,
        FrameTiming::host(0),
        8,
        8,
        FrameFormat::Rgb24,
        &small,
    )
    .await
    .unwrap();
    // A bigger frame forces a ring resize without error.
    let big = vec![7u8; FrameFormat::Rgb24.frame_bytes(16, 16)];
    let d = e
        .publish_frame(
            "ring-grow",
            2,
            FrameTiming::host(0),
            16,
            16,
            FrameFormat::Rgb24,
            &big,
        )
        .await
        .unwrap();
    assert_eq!(d.width, 16);
    let mut out = Vec::new();
    e.read_frame_into(&d, &mut out).await.unwrap();
    assert_eq!(out, big);
    // A consumer maps the grown ring by the name the descriptor carries,
    // so the outgrown ring's teardown must not have unlinked it.
    #[cfg(target_os = "linux")]
    {
        let dir = std::env::var("ADOS_SHM_DIR").unwrap_or_else(|_| "/dev/shm".into());
        let ring = std::fs::read(std::path::Path::new(&dir).join(&d.shm_name))
            .expect("the grown ring is still named");
        let layout = ados_protocol::framebus::RingLayout::read_header(&ring).unwrap();
        assert_eq!(
            ados_protocol::framebus::read_slot_into(&ring, &layout, d.slot, d.seq, &mut out)
                .unwrap(),
            Some(big.len())
        );
        assert_eq!(out, big);
    }
}

#[tokio::test]
async fn a_frame_too_large_for_the_ring_budget_is_refused() {
    // 1080p rgb24 is ~6.2 MB per slot, so a 4 MB budget cannot hold even
    // the two-slot floor. The u16 slot-count header field would have
    // accepted this ring; the byte budget is what actually bounds the
    // tmpfs the board has, so the publish is refused rather than mapped.
    let e = VisionEngine::new(Box::new(MockBackend), RingSizing::new(4, 4 * 1024 * 1024));
    let frame = vec![0u8; FrameFormat::Rgb24.frame_bytes(1920, 1080)];
    let err = e
        .publish_frame(
            "budget-refuse",
            1,
            FrameTiming::host(0),
            1920,
            1080,
            FrameFormat::Rgb24,
            &frame,
        )
        .await
        .expect_err("a frame the budget cannot hold two of must be refused");
    assert!(err.to_string().contains("does not fit"), "{err}");
}

#[tokio::test]
async fn ring_depth_is_reduced_to_fit_the_byte_budget() {
    // A depth of 64 at 640x480 rgb24 is ~59 MB. Under an 8 MB budget the
    // engine streams at the depth that fits instead of exhausting /dev/shm.
    const BUDGET: usize = 8 * 1024 * 1024;
    let frame_bytes = FrameFormat::Rgb24.frame_bytes(640, 480);
    let fitted = fit_slot_count(64, frame_bytes as u32, BUDGET).unwrap();
    assert!(
        fitted < 64,
        "the budget must bite for this test to mean anything"
    );

    let e = VisionEngine::new(Box::new(MockBackend), RingSizing::new(64, BUDGET));
    let frame = vec![0u8; frame_bytes];
    let mut slots = Vec::new();
    for id in 1..=(fitted as u64 + 1) {
        let d = e
            .publish_frame(
                "budget-fit",
                id,
                FrameTiming::host(0),
                640,
                480,
                FrameFormat::Rgb24,
                &frame,
            )
            .await
            .unwrap();
        slots.push(d.slot);
    }
    assert!(
        slots.iter().all(|s| *s < fitted),
        "no descriptor may name a slot outside the ring that was created: {slots:?}"
    );
    // It recycles at the reduced depth: the ring really is `fitted` deep.
    assert_eq!(slots[0], slots[fitted as usize]);
}
