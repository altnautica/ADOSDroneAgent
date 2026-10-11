//! The bounded stream queue and the publish counters.

use super::*;

/// A bounded, drop-oldest queue between the socket's connection tasks and the
/// broker publisher. `push` never waits beyond a short lock.
pub struct StreamQueue {
    pub(super) inner: Mutex<QueueInner>,
    pub(super) ready: Notify,
    pub(super) max_messages: usize,
    pub(super) max_bytes: usize,
    pub(super) dropped_oldest: AtomicU64,
}

#[derive(Default)]
pub(super) struct QueueInner {
    pub(super) items: VecDeque<StreamMessage>,
    pub(super) bytes: usize,
}

impl StreamQueue {
    pub fn new(max_messages: usize, max_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(QueueInner::default()),
            ready: Notify::new(),
            max_messages,
            max_bytes,
            dropped_oldest: AtomicU64::new(0),
        }
    }

    /// Enqueue `msg`, evicting the oldest messages until it fits. Returns how
    /// many were evicted.
    pub fn push(&self, msg: StreamMessage) -> usize {
        let cost = msg.cost();
        let mut evicted = 0;
        {
            let mut q = self.inner.lock();
            while !q.items.is_empty()
                && (q.items.len() >= self.max_messages || q.bytes + cost > self.max_bytes)
            {
                if let Some(old) = q.items.pop_front() {
                    q.bytes -= old.cost();
                    evicted += 1;
                }
            }
            q.bytes += cost;
            q.items.push_back(msg);
        }
        if evicted > 0 {
            self.dropped_oldest
                .fetch_add(evicted as u64, Ordering::Relaxed);
        }
        self.ready.notify_one();
        evicted
    }

    /// Take the oldest message, if any.
    pub fn try_pop(&self) -> Option<StreamMessage> {
        let mut q = self.inner.lock();
        let msg = q.items.pop_front()?;
        q.bytes -= msg.cost();
        Some(msg)
    }

    /// Wait for the next message. Single consumer.
    pub async fn pop(&self) -> StreamMessage {
        loop {
            if let Some(msg) = self.try_pop() {
                return msg;
            }
            self.ready.notified().await;
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Messages evicted to make room since start.
    pub fn dropped_oldest(&self) -> u64 {
        self.dropped_oldest.load(Ordering::Relaxed)
    }
}

/// The lane's counters since start.
#[derive(Debug, Default)]
pub struct PublishCounters {
    pub streams_queued: AtomicU64,
    pub streams_published: AtomicU64,
    /// Dropped because the broker session was down when its turn came.
    pub streams_dropped_offline: AtomicU64,
    /// The broker client's own request queue was full.
    pub streams_publish_failed: AtomicU64,
    pub records_written: AtomicU64,
    pub records_failed: AtomicU64,
    /// Requests refused before any cloud work: bad frame, contract breach,
    /// unpaired, no broker, busy.
    pub refused: AtomicU64,
}

impl PublishCounters {
    pub(super) fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn snapshot(&self, dropped_oldest: u64) -> [u64; 8] {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        [
            get(&self.streams_queued),
            get(&self.streams_published),
            dropped_oldest,
            get(&self.streams_dropped_offline),
            get(&self.streams_publish_failed),
            get(&self.records_written),
            get(&self.records_failed),
            get(&self.refused),
        ]
    }
}
