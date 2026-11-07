//! `WS /api/v1/nav/stream`: the live nav kinds and the follow events.
//!
//! On connect the client gets the latest frame of every kind that has one.
//! After that each change is coalesced: the stream flushes at most once per
//! [`FLUSH_INTERVAL`], sending only the latest value of each kind that changed,
//! so no kind exceeds 20 Hz however fast its producer publishes. Follow events
//! are discrete and go out as they arrive. Keepalive and peer-silence handling
//! are the ground-station streams' ([`crate::routes::gs_ws`]).
//!
//! The client may send `{"type":"view","camera_id","layers":[…]}` frames: the
//! latest one is its view lease (an empty `camera_id` releases it; empty
//! `layers` release it only when it names the leased camera) (refreshed by re-sending every 2 s, lapsing
//! after [`super::VIEW_LEASE_TTL`]). It narrows the per-camera kinds this client
//! receives and feeds the hub's view demand. Any other client frame is ignored.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;

use super::hub::{ClientView, NavKind, Sent, KIND_COUNT};
use super::view::{LeaseHolder, ViewLease, ViewMsg, VIEW_LEASE_TTL};
use super::NavHub;
use crate::routes::gs_ws::{
    client_still_open, keepalive_beat, keepalive_ticker, send_json, WsAuth,
};

/// The shortest gap between two flushes: the per-kind 20 Hz ceiling (the
/// overlay kinds run at 20 Hz while the view is moving fast).
pub(crate) const FLUSH_INTERVAL: Duration = Duration::from_millis(50);

/// Stream connection ids, so each connection holds its own lease.
static NEXT_CLIENT: AtomicU64 = AtomicU64::new(1);

/// Serve one admitted nav stream until the client goes away.
pub(crate) async fn nav_stream_loop(mut socket: WebSocket, hub: NavHub, _auth: WsAuth) {
    let holder = LeaseHolder::Stream(NEXT_CLIENT.fetch_add(1, Ordering::Relaxed));
    let mut lease: Option<ViewLease> = None;
    let mut lease_at = Instant::now();
    let mut changes = hub.watch();
    let mut follow_events = hub.follow_events();
    let mut sent: Sent = [0u64; KIND_COUNT];
    for frame in hub.frames_since(&mut sent, ClientView::Lease(None)) {
        if !send_json(&mut socket, &frame).await {
            return;
        }
    }
    let mut last_flush = Instant::now();
    let mut dirty = false;
    let mut keepalive = keepalive_ticker();
    let mut last_heard = Instant::now();
    loop {
        let flush_at = last_flush + FLUSH_INTERVAL;
        let lease_expiry = lease_at + VIEW_LEASE_TTL;
        tokio::select! {
            changed = changes.changed() => {
                if changed.is_err() {
                    break;
                }
                dirty = true;
            }
            _ = tokio::time::sleep_until(flush_at), if dirty => {
                dirty = false;
                last_flush = Instant::now();
                let mut ok = true;
                for frame in hub.frames_since(&mut sent, ClientView::Lease(lease.as_ref())) {
                    if !send_json(&mut socket, &frame).await {
                        ok = false;
                        break;
                    }
                }
                if !ok {
                    break;
                }
            }
            _ = tokio::time::sleep_until(lease_expiry), if lease.is_some() => {
                // The client stopped refreshing: its view is gone.
                lease = None;
                hub.drop_view(&holder);
            }
            event = follow_events.recv() => match event {
                Ok(frame) => {
                    if !send_json(&mut socket, &frame).await {
                        break;
                    }
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            },
            incoming = socket.recv() => {
                if !client_still_open(&incoming) {
                    break;
                }
                last_heard = Instant::now();
                if let Some(Ok(Message::Text(text))) = &incoming {
                    let msg = serde_json::from_str::<serde_json::Value>(text.as_str())
                        .ok()
                        .as_ref()
                        .and_then(ViewMsg::from_frame);
                    if let Some(msg) = msg {
                        lease_at = Instant::now();
                        if msg.apply(&mut lease) {
                            // A new camera or layer set, or a release: resend
                            // the per-camera kinds narrowed to it.
                            for kind in [NavKind::Ar, NavKind::Obstacles, NavKind::Depth, NavKind::Surfaces] {
                                sent[kind as usize] = 0;
                            }
                            dirty = true;
                        }
                        match &lease {
                            Some(l) => hub.put_view(holder.clone(), l.clone()),
                            None => hub.drop_view(&holder),
                        }
                    }
                }
            }
            _ = keepalive.tick() => {
                if !keepalive_beat(&mut socket, last_heard).await {
                    break;
                }
            }
        }
    }
    hub.drop_view(&holder);
}
