//! Backpressure semantics on the encoder broadcast channel.
//!
//! The lite agent fans encoded frames out to multiple consumers (RTSP
//! pusher, on-disk recorder, MQTT cloud relay) via
//! `tokio::sync::broadcast`. The contract: the producer never blocks;
//! a slow consumer that falls behind sees `RecvError::Lagged(n)`
//! reporting how many frames it missed, and resumes from the head of
//! the queue.
//!
//! These tests exercise that contract with the same channel shape the
//! production pipeline uses (small capacity, slow consumer, fast
//! producer) so any future regression where a consumer back-pressures
//! the encoder surfaces here.

use std::time::Duration;

use ados_video::EncodedFrame;
use tokio::sync::broadcast;
use tokio::time::{sleep, timeout};

const CHANNEL_CAPACITY: usize = 8;
const FRAMES_TO_SEND: usize = 100;
const SLOW_CONSUMER_DELAY: Duration = Duration::from_millis(1);

fn dummy_frame(idx: usize) -> EncodedFrame {
    // Body is a small Annex-B-shaped buffer so the EncodedFrame is
    // structurally valid even though no consumer here actually parses
    // it. Keeping the body small avoids dominating the test runtime.
    let mut bytes = vec![0u8, 0, 0, 1, 0x41];
    bytes.extend_from_slice(&(idx as u32).to_be_bytes());
    EncodedFrame {
        bytes,
        is_keyframe: idx == 0,
        pts_ms: idx as u64,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_consumer_sees_lagged_error_and_resumes() {
    let (tx, mut rx) = broadcast::channel::<EncodedFrame>(CHANNEL_CAPACITY);

    // Producer task: blast all frames as fast as it can.
    let producer_tx = tx.clone();
    let producer = tokio::spawn(async move {
        for i in 0..FRAMES_TO_SEND {
            // `send` returns the number of receivers it reached. We do
            // not assert on that here; the contract under test is that
            // it never blocks and never panics, even when the receiver
            // is far behind.
            let _ = producer_tx.send(dummy_frame(i));
        }
    });

    // Consumer task: drain at SLOW_CONSUMER_DELAY pacing. Records the
    // number of `Lagged` errors and the largest single skip count.
    let mut lagged_events = 0usize;
    let mut max_skip = 0u64;
    let mut received_frames = 0usize;

    let consumer = async {
        loop {
            match rx.recv().await {
                Ok(_frame) => {
                    received_frames += 1;
                    sleep(SLOW_CONSUMER_DELAY).await;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    lagged_events += 1;
                    if n > max_skip {
                        max_skip = n;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
            // Stop once the producer has finished AND the channel is
            // drained. The receiver's `len()` reports queued items.
            if rx.is_empty() && producer.is_finished() {
                break;
            }
            // Hard ceiling so the test cannot loop forever even if
            // the contract is broken.
            if received_frames + lagged_events > FRAMES_TO_SEND * 2 {
                break;
            }
        }
    };

    // Bound the whole exercise so a structural regression cannot hang
    // the test suite.
    timeout(Duration::from_secs(10), consumer)
        .await
        .expect("consumer task must finish within 10s");

    assert!(
        lagged_events > 0,
        "expected at least one Lagged error from the slow consumer",
    );
    assert!(
        max_skip > 0,
        "Lagged error count must report a positive skip value",
    );

    // Producer must have finished feeding all frames despite the slow
    // consumer. If the broadcast were back-pressuring the producer
    // this `await` would hang.
    timeout(Duration::from_secs(2), producer)
        .await
        .expect("producer must finish")
        .expect("producer task must not panic");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn producer_never_blocks_when_no_subscribers_exist() {
    // Drop the receiver before sending to confirm the producer side
    // surfaces a `SendError` cleanly rather than blocking. The cloud-
    // relay disconnect path relies on this: no subscribers means the
    // encoder loop should keep running, not stall.
    let (tx, rx) = broadcast::channel::<EncodedFrame>(CHANNEL_CAPACITY);
    drop(rx);

    let send_round = async {
        for i in 0..FRAMES_TO_SEND {
            // `send` returns `Err(SendError)` when there are no live
            // receivers; that is fine, the encoder ignores it.
            let _ = tx.send(dummy_frame(i));
        }
    };

    timeout(Duration::from_secs(2), send_round)
        .await
        .expect("send loop must not block when no receivers exist");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fast_consumer_sees_all_frames_without_lag() {
    // Sanity sibling to the slow-consumer test. With a fast consumer
    // and a generous channel capacity, every frame must arrive.
    let (tx, mut rx) = broadcast::channel::<EncodedFrame>(FRAMES_TO_SEND * 2);

    let producer = tokio::spawn(async move {
        for i in 0..FRAMES_TO_SEND {
            let _ = tx.send(dummy_frame(i));
        }
    });

    let mut count = 0usize;
    while count < FRAMES_TO_SEND {
        match rx.recv().await {
            Ok(_) => count += 1,
            Err(broadcast::error::RecvError::Lagged(_)) => {
                panic!("fast consumer must not lag with oversized capacity");
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    assert_eq!(count, FRAMES_TO_SEND);

    timeout(Duration::from_secs(2), producer)
        .await
        .expect("producer must finish")
        .expect("producer task must not panic");
}
