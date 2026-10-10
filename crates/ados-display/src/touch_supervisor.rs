//! Never-give-up supervision of the touchscreen reader.
//!
//! A panel without a digitizer, a late-loading ADS7846 module, a USB touch
//! replug or a transient `ENODEV` must not leave touch dead until the unit
//! restarts. [`supervise_touch`] runs discover → drain in a loop: when no node
//! is found or the open device errors, it marks touch absent, waits the fixed
//! local-socket pace and rediscovers. It returns only when the render loop has
//! dropped its receiver, so the touch sender stays alive for the life of the
//! UI and the render loop's touch arm never resolves to a stream of `None`s.
//!
//! The loop is generic over the discovery and drain steps so the recovery
//! behaviour is unit-tested off-target; the evdev specifics live in
//! [`crate::touch_input`].

use std::future::Future;

use tokio::sync::mpsc::Sender;

use crate::touch_input::{TouchEvent, TouchTransformHandle};

/// How one open device's session ended.
#[derive(Debug)]
pub enum SessionEnd {
    /// The render loop dropped its receiver: stop for good.
    ChannelClosed,
    /// The device errored or vanished: rediscover.
    Lost(std::io::Error),
}

/// Discover a touch device, drain it, and on absence or loss mark touch absent,
/// wait [`ados_protocol::retry::LOCAL_SOCKET`] and rediscover. Returns only once
/// `tx` is closed.
pub async fn supervise_touch<D, Disc, Drain, Fut>(
    handle: TouchTransformHandle,
    tx: Sender<TouchEvent>,
    mut discover: Disc,
    mut drain: Drain,
) where
    Disc: FnMut() -> Option<D>,
    Drain: FnMut(D, TouchTransformHandle, Sender<TouchEvent>) -> Fut,
    Fut: Future<Output = SessionEnd>,
{
    // Log the absent state once per transition, not every retry.
    let mut absence_logged = false;
    loop {
        if tx.is_closed() {
            return;
        }
        match discover() {
            Some(device) => {
                absence_logged = false;
                match drain(device, handle.clone(), tx.clone()).await {
                    SessionEnd::ChannelClosed => {
                        handle.mark_absent();
                        tracing::info!("touch gesture channel closed; stopping reader");
                        return;
                    }
                    SessionEnd::Lost(e) => {
                        tracing::warn!(error = %e, "touchscreen lost; rediscovering");
                    }
                }
            }
            None => {
                if !absence_logged {
                    tracing::info!("no touchscreen evdev node found; retrying discovery");
                    absence_logged = true;
                }
            }
        }
        handle.mark_absent();
        if tx.is_closed() {
            return;
        }
        tokio::time::sleep(ados_protocol::retry::LOCAL_SOCKET.wait()).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use ados_hid::touch::{GestureKind, TouchGesture};
    use tokio::sync::mpsc;

    use super::*;

    fn tap() -> TouchEvent {
        TouchEvent {
            gesture: TouchGesture {
                kind: GestureKind::Tap,
                start_x: 10,
                start_y: 10,
                end_x: 10,
                end_y: 10,
                start_t_ms: 0,
                end_t_ms: 50,
                duration_ms: 50,
                direction: None,
                velocity_px_per_s: 0.0,
                samples: Vec::new(),
            },
            raw: (100, 200),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn touch_rediscovers_after_error() {
        let handle = TouchTransformHandle::new(0);
        let (tx, mut rx) = mpsc::channel::<TouchEvent>(4);
        let discoveries = Arc::new(AtomicUsize::new(0));
        let sessions = Arc::new(AtomicUsize::new(0));

        let disc_count = discoveries.clone();
        let sess_count = sessions.clone();
        let task = tokio::spawn(supervise_touch(
            handle.clone(),
            tx,
            move || {
                // Absent on the first look, present after that.
                let n = disc_count.fetch_add(1, Ordering::SeqCst);
                (n > 0).then_some(n)
            },
            move |_dev: usize, handle: TouchTransformHandle, tx: Sender<TouchEvent>| {
                let n = sess_count.fetch_add(1, Ordering::SeqCst);
                async move {
                    handle.mark_present();
                    if n == 0 {
                        // The first open device fails mid-read.
                        return SessionEnd::Lost(std::io::Error::other("ENODEV"));
                    }
                    if tx.send(tap()).await.is_err() {
                        return SessionEnd::ChannelClosed;
                    }
                    // Hold the device open until the receiver goes away.
                    tx.closed().await;
                    SessionEnd::ChannelClosed
                }
            },
        ));

        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("touch recovered within the retry pace")
            .expect("reader still alive");
        assert_eq!(event.raw, (100, 200));
        // Absent once, lost once, then the third discovery delivered the tap.
        assert_eq!(discoveries.load(Ordering::SeqCst), 3);
        assert_eq!(sessions.load(Ordering::SeqCst), 2);
        assert!(handle.touch_present());

        drop(rx);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("supervisor stops once the receiver is dropped")
            .unwrap();
        assert!(!handle.touch_present(), "a closed session clears presence");
    }

    #[tokio::test(start_paused = true)]
    async fn select_loop_bounded_after_touch_sender_drop() {
        // The render loop's shape: a fixed tick plus the touch arm guarded by
        // `touch_open`. A dropped sender resolves the arm to `None` once; the
        // guard then disables it, so the loop wakes only on the tick instead of
        // spinning on an always-ready closed channel.
        let tick_period = crate::render_loop::TICK_GRANULARITY;
        let (tx, mut touch_rx) = mpsc::channel::<TouchEvent>(4);
        drop(tx);

        let start = tokio::time::Instant::now();
        let deadline = start + Duration::from_secs(1);
        let mut tick = tokio::time::interval_at(start + tick_period, tick_period);
        let mut touch_open = true;
        let mut iterations = 0u32;
        loop {
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => break,
                _ = tick.tick() => {}
                maybe = touch_rx.recv(), if touch_open => {
                    if maybe.is_none() {
                        touch_open = false;
                    }
                }
            }
            iterations += 1;
            assert!(
                iterations <= 10,
                "select loop spun {iterations} times in 1 s"
            );
        }
        assert!(!touch_open);
        assert!(iterations <= 10);
    }
}
