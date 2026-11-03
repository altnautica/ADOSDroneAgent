//! `WS /api/v1/nav/stream`: the live nav kinds and the follow events.
//!
//! On connect the client gets the latest frame of every kind that has one.
//! After that each change is coalesced: the stream flushes at most once per
//! [`FLUSH_INTERVAL`], sending only the latest value of each kind that changed,
//! so no kind exceeds 10 Hz however fast its producer publishes. Follow events
//! are discrete and go out as they arrive. Keepalive and peer-silence handling
//! are the ground-station streams' ([`crate::routes::gs_ws`]).

use std::time::Duration;

use axum::extract::ws::WebSocket;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;

use super::NavHub;
use crate::routes::gs_ws::{
    client_still_open, keepalive_beat, keepalive_ticker, send_json, WsAuth,
};

/// The shortest gap between two flushes: the per-kind 10 Hz ceiling.
pub(crate) const FLUSH_INTERVAL: Duration = Duration::from_millis(100);

/// Serve one admitted nav stream until the client goes away.
pub(crate) async fn nav_stream_loop(mut socket: WebSocket, hub: NavHub, _auth: WsAuth) {
    let mut changes = hub.watch();
    let mut follow_events = hub.follow_events();
    let mut sent = [0u64; 5];
    for frame in hub.frames_since(&mut sent) {
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
        tokio::select! {
            changed = changes.changed() => {
                if changed.is_err() {
                    return;
                }
                dirty = true;
            }
            _ = tokio::time::sleep_until(flush_at), if dirty => {
                dirty = false;
                last_flush = Instant::now();
                for frame in hub.frames_since(&mut sent) {
                    if !send_json(&mut socket, &frame).await {
                        return;
                    }
                }
            }
            event = follow_events.recv() => match event {
                Ok(frame) => {
                    if !send_json(&mut socket, &frame).await {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return,
            },
            incoming = socket.recv() => {
                if !client_still_open(&incoming) {
                    return;
                }
                last_heard = Instant::now();
            }
            _ = keepalive.tick() => {
                if !keepalive_beat(&mut socket, last_heard).await {
                    return;
                }
            }
        }
    }
}
