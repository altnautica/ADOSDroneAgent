use super::*;
use std::cell::{Cell, RefCell};

/// A publisher with a fixed number of free request slots, recording what
/// it accepted. `free` models the client's request channel room.
#[derive(Default)]
struct FakeClient {
    free: Cell<usize>,
    published: RefCell<Vec<Vec<u8>>>,
}

impl FramePublisher for FakeClient {
    fn try_publish_frame(&self, _topic: &str, payload: Vec<u8>) -> Result<(), ()> {
        if self.free.get() == 0 {
            return Err(());
        }
        self.free.set(self.free.get() - 1);
        self.published.borrow_mut().push(payload);
        Ok(())
    }
}

const TOPIC: &str = "ados/d/mavlink/tx";

#[tokio::test]
async fn frames_queued_while_the_broker_is_down_are_dropped_not_published() {
    // During an outage the relay must not hand telemetry to the client: it
    // would sit in the request queue and reach the GCS minutes late, as if
    // live, once the session came back.
    let client = FakeClient::default();
    client.free.set(100);
    let mut queue = BoundedPublishQueue::with_capacity(10);
    for i in 0..3u8 {
        queue.push(vec![i]);
    }
    let mut metrics = RelayMetrics::default();
    let down = AtomicBool::new(false);
    drain_publish_queue(&mut queue, &mut metrics, &client, &down, TOPIC);
    assert_eq!(metrics.frames_dropped_not_connected, 3);
    assert!(client.published.borrow().is_empty());
    assert!(queue.is_empty());
}

#[tokio::test]
async fn queue_publishes_newest_after_backpressure() {
    // The client takes nothing: frames stay queued and the bound keeps the
    // newest. Once the client drains, the newest frames are what go out.
    let client = FakeClient::default();
    let capacity = 4usize;
    let mut queue = BoundedPublishQueue::with_capacity(capacity);
    let mut metrics = RelayMetrics::default();
    let up = AtomicBool::new(true);
    for i in 0..(3 * capacity as u8) {
        if queue.push(vec![i]) {
            metrics.frames_dropped_queue_full += 1;
        }
        drain_publish_queue(&mut queue, &mut metrics, &client, &up, TOPIC);
    }
    assert!(client.published.borrow().is_empty());
    assert_eq!(queue.len(), capacity);
    assert_eq!(metrics.frames_dropped_queue_full, 2 * capacity as u64);

    client.free.set(10);
    drain_publish_queue(&mut queue, &mut metrics, &client, &up, TOPIC);
    assert_eq!(*client.published.borrow(), vec![vec![8u8, 9, 10, 11]]);
    assert_eq!(metrics.frames_published, capacity as u64);
    assert!(queue.is_empty());
}

#[tokio::test]
async fn a_refused_publish_keeps_its_frames_for_the_next_drain() {
    let client = FakeClient::default();
    let mut queue = BoundedPublishQueue::with_capacity(10);
    let mut metrics = RelayMetrics::default();
    let up = AtomicBool::new(true);
    queue.push(vec![1, 2]);
    drain_publish_queue(&mut queue, &mut metrics, &client, &up, TOPIC);
    assert_eq!(queue.len(), 1);
    assert_eq!(metrics.publish_errors, 1);
    client.free.set(1);
    drain_publish_queue(&mut queue, &mut metrics, &client, &up, TOPIC);
    assert_eq!(*client.published.borrow(), vec![vec![1u8, 2]]);
}

#[tokio::test(start_paused = true)]
async fn batching_concatenates_within_50ms() {
    let client = FakeClient::default();
    client.free.set(10);
    let mut queue = BoundedPublishQueue::with_capacity(100);
    let mut metrics = RelayMetrics::default();
    let up = AtomicBool::new(true);
    let start = Instant::now();
    queue.push(vec![0xFD, 1]);
    tokio::time::advance(Duration::from_millis(10)).await;
    queue.push(vec![0xFD, 2]);
    tokio::time::advance(Duration::from_millis(10)).await;
    queue.push(vec![0xFE, 3]);
    // Inside the window and under the byte budget: not due yet.
    assert!(!queue.batch_due(Instant::now()));
    assert_eq!(queue.flush_deadline(), Some(start + BATCH_WINDOW));
    tokio::time::advance(Duration::from_millis(30)).await;
    assert!(queue.batch_due(Instant::now()));
    drain_publish_queue(&mut queue, &mut metrics, &client, &up, TOPIC);
    assert_eq!(
        *client.published.borrow(),
        vec![vec![0xFD, 1, 0xFD, 2, 0xFE, 3]]
    );
    assert_eq!(metrics.frames_published, 3);
    assert_eq!(queue.flush_deadline(), None);
}

#[tokio::test]
async fn a_full_byte_budget_is_due_at_once_and_splits_publishes() {
    let client = FakeClient::default();
    client.free.set(10);
    let mut queue = BoundedPublishQueue::with_capacity(100);
    let mut metrics = RelayMetrics::default();
    let up = AtomicBool::new(true);
    let frame = vec![0u8; 1500];
    for _ in 0..3 {
        queue.push(frame.clone());
    }
    assert!(queue.batch_due(Instant::now()));
    drain_publish_queue(&mut queue, &mut metrics, &client, &up, TOPIC);
    let sizes: Vec<usize> = client.published.borrow().iter().map(Vec::len).collect();
    assert_eq!(sizes, vec![3000, 1500]);
    assert!(sizes.iter().all(|s| *s <= BATCH_MAX_BYTES));
}

#[test]
fn drop_oldest_kicks_in_at_capacity() {
    let mut q = BoundedPublishQueue::with_capacity(3);
    assert!(!q.push(b"a".to_vec()));
    assert!(!q.push(b"b".to_vec()));
    assert!(!q.push(b"c".to_vec()));
    // The 4th push is over capacity: oldest ("a") dropped, "d" appended.
    assert!(q.push(b"d".to_vec()));
    assert_eq!(q.len(), 3);
    assert_eq!(q.bytes(), 3);
    assert_eq!(q.dropped_oldest(), 1);
    assert_eq!(q.peek_batch(usize::MAX), Some((b"bcd".to_vec(), 3)));
}

#[test]
fn drop_oldest_at_the_production_2000_bound() {
    let mut q = BoundedPublishQueue::new();
    for i in 0..QUEUE_MAXSIZE {
        assert!(!q.push(vec![i as u8]), "no drop until full");
    }
    assert_eq!(q.dropped_oldest(), 0);
    assert!(q.push(vec![0xFF]));
    assert_eq!(q.len(), QUEUE_MAXSIZE);
    assert_eq!(q.dropped_oldest(), 1);
}

#[tokio::test]
async fn the_real_client_takes_a_publish_without_a_poller() {
    // An unpolled rumqttc client accepts into its request channel until it
    // is full and never blocks the caller.
    let opts = rumqttc::MqttOptions::new("ados-test", ("127.0.0.1", 1));
    let (client, _eventloop) = rumqttc::AsyncClient::builder(opts).build();
    let mut queue = BoundedPublishQueue::with_capacity(10);
    queue.push(vec![1]);
    let mut metrics = RelayMetrics::default();
    let up = AtomicBool::new(true);
    drain_publish_queue(&mut queue, &mut metrics, &client, &up, TOPIC);
    assert_eq!(metrics.frames_published + metrics.publish_errors, 1);
}
