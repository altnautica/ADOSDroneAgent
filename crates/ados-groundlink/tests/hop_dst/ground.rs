//! A ground station: the production hop follower, presence cache and
//! control-frame dispatch ([`handle_control_frame_on`]), and the production
//! valid-packet watchdog with its channel acquirer, over a simulated receive
//! radio. Around them, the receive loop's generation lifecycle as
//! `daemon/receive.rs` runs it: tune home, a fresh valid-decode counter and
//! watchdog per generation, and after the watchdog terminates the receiver, the
//! 5 s retry with the receive chain down.
//!
//! The watchdog reads presence age from the simulated clock: the presence
//! cache stamps beacons with wall-clock unix seconds, which a paused runtime
//! does not advance. The age is that of the same drone beacons the dispatch
//! records; the announced channel is the cache's own.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use ados_groundlink::acquire::{ChannelAcquirer, ChannelSetter, DWELL_SECONDS, MAX_SWEEP_ROUNDS};
use ados_groundlink::presence::{
    handle_control_frame_on, ControlContext, ControlPort, GsPresenceCache, HopFollower,
};
use ados_groundlink::watchdog::{
    Clock, LockedChannelHint, PresenceCache, RxProcess, ValidPacketWatchdog,
};
use ados_groundlink::wfb_rx::SharedValidCounter;
use ados_radio::hop::{build_presence_beacon, parse_presence_beacon};
use std::future::Future;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration, Instant};

use crate::world::{hop_target, key, lock, transmit, Node, Shared, HOME};

/// `presence.rs` `PRESENCE_CADENCE`.
const PRESENCE_CADENCE: Duration = Duration::from_secs(10);
/// `daemon/receive.rs`: the exit poll and `RETRY_INTERVAL` after a generation.
const EXIT_POLL: Duration = Duration::from_millis(500);
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// The single-drone station the scenarios run: one registered slot.
const SOLO: ControlContext = ControlContext {
    slot: 1,
    fleet_slots: 1,
};

/// Ground `i`'s receive radio, as the follower's and the acquirer's channel
/// setter.
struct Radio {
    w: Shared,
    i: usize,
}

impl ChannelSetter for Radio {
    fn set_channel<'a>(
        &'a self,
        _interface: &'a str,
        channel: u8,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            let (fail, took, prev) = {
                let mut g = lock(&self.w);
                let p = g.f.ground_retune_fail;
                let fail = g.faults_on && g.rng.chance(p);
                let took = g.rng.ms(100, 300);
                let prev = g.grounds[self.i].channel;
                g.grounds[self.i].channel = 0;
                (fail, took, prev)
            };
            sleep(Duration::from_millis(took)).await;
            let mut g = lock(&self.w);
            let i = self.i;
            let name = g.grounds[i].name;
            if fail {
                g.grounds[i].channel = prev;
                g.grounds[i].retune_failed_ms = Some(g.now_ms());
                g.log(format!("{name} retune {channel} FAILED, still on {prev}"));
            } else {
                g.grounds[i].channel = channel;
                if channel != prev {
                    g.log(format!("{name} retune {prev} -> {channel}"));
                }
            }
            !fail
        })
    }

    fn live_channel<'a>(
        &'a self,
        _interface: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<u8>> + Send + 'a>> {
        Box::pin(async move {
            let ch = lock(&self.w).grounds[self.i].channel;
            (ch != 0).then_some(ch)
        })
    }
}

/// Ground `i`'s control port: the ack goes back on the air on the channel the
/// radio is on; the permitted set is the world's.
struct Port {
    w: Shared,
    i: usize,
}

impl ControlPort for Port {
    fn send_ack<'a>(&'a self, frame: &'a [u8]) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let mut g = lock(&self.w);
            if let Some(t) = hop_target(frame) {
                if let Some(r) = g.record_for(t) {
                    if g.hops[r].acked_by.insert(self.i) {
                        g.stats.follows += 1;
                        let name = g.grounds[self.i].name;
                        g.log(format!("{name} acks hop -> {t}"));
                    }
                }
            }
            transmit(&self.w, &mut g, Node::Ground(self.i), frame);
        })
    }

    fn permitted<'a>(
        &'a self,
        _iface: &'a str,
    ) -> Pin<Box<dyn Future<Output = BTreeSet<u8>> + Send + 'a>> {
        Box::pin(async move { lock(&self.w).grounds[self.i].permitted.clone() })
    }
}

/// The simulated monotonic clock, seconds.
struct SimClock(Instant);

impl Clock for SimClock {
    fn monotonic(&self) -> f64 {
        Instant::now().duration_since(self.0).as_secs_f64()
    }
}

/// The receive chain of the current generation.
struct Rx {
    w: Shared,
    i: usize,
    kills: AtomicU32,
}

impl RxProcess for Rx {
    fn is_running(&self) -> bool {
        lock(&self.w).grounds[self.i].rx_alive
    }

    fn terminate(&self) {
        self.kills.fetch_add(1, Ordering::Relaxed);
        lock(&self.w).grounds[self.i].rx_alive = false;
    }

    fn terminate_count(&self) -> u32 {
        self.kills.load(Ordering::Relaxed)
    }
}

/// Presence for the watchdog: the age of the last drone beacon on the
/// simulated clock, and the cache's announced channel.
struct Presence {
    w: Shared,
    i: usize,
    cache: GsPresenceCache,
}

impl PresenceCache for Presence {
    fn presence_age_s(&self) -> Option<f64> {
        let g = lock(&self.w);
        let now = g.now_ms();
        g.grounds[self.i]
            .peer_seen_ms
            .map(|t| (now - t) as f64 / 1000.0)
    }

    fn announced_channel(&self) -> Option<u8> {
        self.cache.peer_channel()
    }
}

struct NoHint;

impl LockedChannelHint for NoHint {
    fn persist(&self, _channel: u8) {}
}

/// Start ground `i`: the control-plane listener, the beacon and the receive
/// loop.
pub fn spawn(
    w: &Shared,
    i: usize,
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let name = lock(w).grounds[i].name;
    let radio = Arc::new(Radio { w: w.clone(), i });
    let group = Arc::new(tokio::sync::Mutex::new(vec![name.to_string()]));
    let follower = HopFollower::new(radio.clone(), group);
    let cache = GsPresenceCache::new();
    vec![
        tokio::spawn(listener(w.clone(), i, rx, cache.clone(), follower.clone())),
        tokio::spawn(beacons(w.clone(), i)),
        tokio::spawn(receive_loop(w.clone(), i, radio, cache, follower)),
    ]
}

/// `listen_on_slot`: every frame through the production dispatch.
async fn listener(
    w: Shared,
    i: usize,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    cache: GsPresenceCache,
    follower: HopFollower,
) {
    let key = key();
    let port = Port { w: w.clone(), i };
    let name = lock(&w).grounds[i].name;
    while let Some(pkt) = rx.recv().await {
        if let Some(p) = parse_presence_beacon(&pkt, &key) {
            if p.role == "drone" {
                let mut g = lock(&w);
                let now = g.now_ms();
                g.grounds[i].peer_seen_ms = Some(now);
            }
        }
        handle_control_frame_on(&port, &pkt, &key, &cache, name, Some(&follower), SOLO).await;
    }
}

/// `emit_loop`: the ground's beacon every 10 s, advertising the configured
/// channel.
async fn beacons(w: Shared, i: usize) {
    let key = key();
    let mut tick = tokio::time::interval(PRESENCE_CADENCE);
    loop {
        tick.tick().await;
        let mut g = lock(&w);
        let name = g.grounds[i].name;
        let pkt = build_presence_beacon(name, false, HOME, 0, 0, &key);
        transmit(&w, &mut g, Node::Ground(i), &pkt);
    }
}

/// The receive loop's generations.
async fn receive_loop(
    w: Shared,
    i: usize,
    radio: Arc<Radio>,
    cache: GsPresenceCache,
    follower: HopFollower,
) {
    let (name, permitted) = {
        let g = lock(&w);
        (g.grounds[i].name, g.grounds[i].permitted.clone())
    };
    let clock: Arc<dyn Clock> = Arc::new(SimClock(lock(&w).t0));
    let presence: Arc<dyn PresenceCache> = Arc::new(Presence {
        w: w.clone(),
        i,
        cache,
    });
    loop {
        // `prepare_interfaces`: tune the rendezvous home.
        radio.set_channel(name, HOME).await;
        let counter = SharedValidCounter::new();
        follower.set_valid_counter(counter.clone());
        {
            let mut g = lock(&w);
            g.grounds[i].counter = Some(counter.clone());
            g.grounds[i].rx_alive = true;
        }
        let rx = Arc::new(Rx {
            w: w.clone(),
            i,
            kills: AtomicU32::new(0),
        });
        let acquirer = ChannelAcquirer::new(
            name,
            "u-nii-3",
            Arc::new(counter),
            radio.clone(),
            DWELL_SECONDS,
            MAX_SWEEP_ROUNDS,
            Some(permitted.clone()),
        );
        let mut watchdog = ValidPacketWatchdog::new(
            name,
            HOME,
            HOME,
            clock.clone(),
            rx.clone(),
            presence.clone(),
            Arc::new(NoHint),
            acquirer,
        );
        watchdog.run().await;
        {
            let mut g = lock(&w);
            g.grounds[i].rx_alive = false;
            g.stats.rx_restarts += 1;
            g.log(format!(
                "{name} watchdog: reacquire failed, receive restart"
            ));
        }
        sleep(EXIT_POLL).await;
        sleep(RETRY_INTERVAL).await;
    }
}

/// The valid-decode counters: every 100 ms each ground whose receive chain is
/// up and tuned to the live drone's channel decodes its video, less the loss.
pub async fn traffic(w: Shared) {
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tick.tick().await;
        let mut g = lock(&w);
        let ch = g.drone.channel;
        if !g.drone.alive || ch == 0 || g.outage {
            continue;
        }
        let loss = if g.faults_on { g.f.loss } else { 0.0 };
        for i in 0..g.grounds.len() {
            let tuned = g.grounds[i].rx_alive && g.grounds[i].channel == ch;
            if tuned && !g.rng.chance(loss) {
                if let Some(c) = &g.grounds[i].counter {
                    c.add(10);
                }
            }
        }
    }
}
