//! The simulated world: the drone's radio, one or two ground stations, the air
//! between them, the fault plan and the trace. One value behind one lock; every
//! actor reads and writes it on the runtime's paused clock, so a seed names a
//! run exactly.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard};

use ados_groundlink::wfb_rx::SharedValidCounter;
use ados_radio::hop::{derive_pair_key, parse_hop_announce};
use ados_radio::radio_cmd::ManualHopRequest;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use crate::rng::Rng;

/// The rendezvous home channel both sides start on.
pub const HOME: u8 = 149;
/// The configured band (U-NII-3), the drone's regulatory-enabled set.
pub const BAND: [u8; 5] = [149, 153, 157, 161, 165];
/// The cold-start pair key both sides authenticate under.
pub fn key() -> [u8; 32] {
    derive_pair_key(None)
}

/// The world, shared by every actor.
pub type Shared = Arc<Mutex<World>>;

/// Lock the world. A panic elsewhere already fails the run, so a poisoned lock
/// is taken as is.
pub fn lock(w: &Shared) -> MutexGuard<'_, World> {
    w.lock().unwrap_or_else(|e| e.into_inner())
}

/// What goes wrong in a run, and how often. Every draw comes from the run's
/// one generator.
#[derive(Clone, Debug)]
pub struct Faults {
    /// Ground stations hearing the drone (one or two).
    pub grounds: usize,
    /// Per-frame loss on the control plane, both directions.
    pub loss: f32,
    /// Extra loss on hop frames from a ground (the HopAck echo).
    pub ack_loss: f32,
    /// Chance a frame arrives twice.
    pub dup: f32,
    /// Air delay per frame, ms (uniform; overlapping ranges reorder frames).
    pub delay_ms: (u64, u64),
    /// Chance a drone `iw set channel` does not land.
    pub drone_retune_fail: f32,
    /// Chance a drone radio-group respawn does not come back.
    pub drone_spawn_fail: f32,
    /// Chance a ground retune does not land.
    pub ground_retune_fail: f32,
    /// Gap between wfb process crashes on the drone, seconds.
    pub crash_every_s: Option<(f32, f32)>,
    /// Gap between RF blackouts, and their length, seconds.
    pub outage: Option<((f32, f32), (f32, f32))>,
    /// Gap between bind windows, and their length, seconds.
    pub bind: Option<((f32, f32), (f32, f32))>,
    /// Gap between operator manual hop requests, seconds.
    pub manual_every_s: Option<(f32, f32)>,
    /// Opt the drone into the periodic hop.
    pub periodic: bool,
    /// A channel the (first) ground station may not tune.
    pub ground_refuses: Option<u8>,
    /// Gap between interference episodes on one channel, seconds.
    pub interference_every_s: (f32, f32),
}

impl Default for Faults {
    fn default() -> Self {
        Self {
            grounds: 1,
            loss: 0.02,
            ack_loss: 0.0,
            dup: 0.0,
            delay_ms: (2, 8),
            drone_retune_fail: 0.0,
            drone_spawn_fail: 0.0,
            ground_retune_fail: 0.0,
            crash_every_s: None,
            outage: None,
            bind: None,
            manual_every_s: None,
            periodic: false,
            ground_refuses: None,
            interference_every_s: (40.0, 80.0),
        }
    }
}

/// The run's trace: one line per event, stamped with simulated milliseconds.
#[derive(Default)]
pub struct Trace {
    pub bytes: Vec<u8>,
}

impl Trace {
    pub fn lines(&self) -> Vec<&str> {
        std::str::from_utf8(&self.bytes)
            .unwrap_or("")
            .lines()
            .collect()
    }

    /// The first line where two traces part, with both lines.
    pub fn first_difference(&self, other: &Trace) -> Option<(usize, String, String)> {
        let (a, b) = (self.lines(), other.lines());
        (0..a.len().max(b.len())).find_map(|i| {
            let (x, y) = (a.get(i).copied(), b.get(i).copied());
            (x != y).then(|| (i, x.unwrap_or("").to_string(), y.unwrap_or("").to_string()))
        })
    }
}

/// The drone's radio as the air sees it.
pub struct DroneRf {
    /// The interface channel; 0 while a retune is in progress (deaf and mute).
    pub channel: u8,
    /// The wfb process group is running (transmits, receives).
    pub alive: bool,
    /// A group respawn is in progress (the service's exit watch is blocked on
    /// the process lock, as in production).
    pub respawning: bool,
    /// The radio-group generation (one per bring-up).
    pub generation: u64,
    /// When the live generation's hop supervisor started, ms.
    pub gen_started_ms: u64,
}

/// One ground station's radio as the air sees it, and the facts its
/// simulated surroundings keep (the production follower, cache and watchdog
/// hold the rest).
pub struct GroundRf {
    pub name: &'static str,
    /// The receive radio's channel; 0 while retuning.
    pub channel: u8,
    pub inbox: mpsc::UnboundedSender<Vec<u8>>,
    /// The receive chain is up (between a generation's start and the
    /// watchdog terminating it): it decodes frames and video.
    pub rx_alive: bool,
    /// The current receive generation's valid-decode counter.
    pub counter: Option<SharedValidCounter>,
    /// The adapter's permitted channel set.
    pub permitted: BTreeSet<u8>,
    /// When a drone beacon was last decoded, ms.
    pub peer_seen_ms: Option<u64>,
    /// When the current drone/ground split began, ms.
    pub split_since: Option<u64>,
    /// When a retune of this radio last failed, ms.
    pub retune_failed_ms: Option<u64>,
}

/// One announced hop as the drone sent it: consecutive announces of one
/// target.
pub struct HopRecord {
    pub target: u8,
    pub first_ms: u64,
    pub last_ms: u64,
    /// The countdown the last announce carried, ms.
    pub last_delay_ms: u64,
    /// When the drone flips if it hears an ack, ms.
    pub flip_ms: u64,
    /// The drone called `set_channel(target)` after announcing (it heard an
    /// ack).
    pub executed: bool,
    /// The target is the channel the drone is on: a call-back after a failed
    /// retune, or a manual hop to where it already is. Not a hop.
    pub own_channel: bool,
    /// Grounds that echoed an ack for one of this hop's announces.
    pub acked_by: BTreeSet<usize>,
}

/// The drone's control-plane return port, held by one listener at a time.
pub struct ReturnPort {
    pub generation: u64,
    pub inbox: mpsc::UnboundedSender<Vec<u8>>,
}

/// Snapshot at a drone crash, for the rejoin check at the next bring-up.
pub struct CrashSnap {
    pub at_ms: u64,
    pub drone_channel: u8,
    pub ground_channels: Vec<u8>,
}

/// Counts for the coverage report.
#[derive(Default, Debug, Clone, Copy)]
pub struct Stats {
    pub hops_announced: u32,
    pub hops_executed: u32,
    pub hops_unacked: u32,
    pub lost_acks: u32,
    /// Hops a ground acked whose every ack the drone missed.
    pub acked_unheard: u32,
    pub own_channel_announces: u32,
    pub drone_retune_fails: u32,
    pub follows: u32,
    /// Receive restarts after a watchdog's failed reacquire.
    pub rx_restarts: u32,
    /// Drone/ground splits that ended with the pair together again.
    pub splits_healed: u32,
    /// Hops acked but never heard by the drone, checked after the window.
    pub unheard_checked: u32,
    pub crashes: u32,
    pub bringups: u32,
    pub return_homes: u32,
    pub outages: u32,
    pub binds: u32,
    pub manual_accepted: u32,
    pub manual_rejected: u32,
    pub hop_checks: u32,
    pub hop_checks_skipped: u32,
    pub rejoin_checks: u32,
}

/// Everything in the simulation.
pub struct World {
    pub rng: Rng,
    pub t0: Instant,
    pub f: Faults,
    /// Faults and interference run until the quiet tail.
    pub faults_on: bool,
    pub trace: Trace,
    pub violations: Vec<String>,
    pub drone: DroneRf,
    pub grounds: Vec<GroundRf>,
    pub port: Option<ReturnPort>,
    pub hops: Vec<HopRecord>,
    /// The live generation's manual-hop request channel.
    pub manual_tx: Option<mpsc::Sender<ManualHopRequest>>,
    pub bind_open: bool,
    pub outage: bool,
    /// The channel under interference now.
    pub bad: Option<u8>,
    /// Last crash, bring-up, outage edge or failed respawn, ms: a hop check
    /// whose window holds one is skipped.
    pub disrupted_ms: Option<u64>,
    pub crash: Option<CrashSnap>,
    /// When the drone's hop state and radio channel began to disagree, ms.
    pub state_split_since: Option<u64>,
    /// When a drone retune or group respawn last failed, ms.
    pub drone_failed_ms: Option<u64>,
    pub stats: Stats,
}

impl World {
    pub fn new(seed: u64, f: Faults, inboxes: Vec<mpsc::UnboundedSender<Vec<u8>>>) -> Self {
        let names = ["gs-a", "gs-b"];
        let grounds = inboxes
            .into_iter()
            .enumerate()
            .map(|(i, inbox)| {
                let mut permitted: BTreeSet<u8> = BAND.into_iter().collect();
                if i == 0 {
                    if let Some(c) = f.ground_refuses {
                        permitted.remove(&c);
                    }
                }
                GroundRf {
                    name: names[i],
                    channel: HOME,
                    inbox,
                    rx_alive: false,
                    counter: None,
                    permitted,
                    peer_seen_ms: None,
                    split_since: None,
                    retune_failed_ms: None,
                }
            })
            .collect();
        Self {
            rng: Rng::new(seed),
            t0: Instant::now(),
            f,
            faults_on: true,
            trace: Trace::default(),
            violations: Vec::new(),
            drone: DroneRf {
                channel: 0,
                alive: false,
                respawning: false,
                generation: 0,
                gen_started_ms: 0,
            },
            grounds,
            port: None,
            hops: Vec::new(),
            manual_tx: None,
            bind_open: false,
            outage: false,
            bad: None,
            disrupted_ms: None,
            crash: None,
            state_split_since: None,
            drone_failed_ms: None,
            stats: Stats::default(),
        }
    }

    pub fn now_ms(&self) -> u64 {
        Instant::now().duration_since(self.t0).as_millis() as u64
    }

    pub fn log(&mut self, line: impl AsRef<str>) {
        let t = self.now_ms();
        let s = format!("{:>7}.{:03} {}\n", t / 1000, t % 1000, line.as_ref());
        self.trace.bytes.extend_from_slice(s.as_bytes());
    }

    pub fn violate(&mut self, what: String) {
        self.log(format!("VIOLATION {what}"));
        let t = self.now_ms();
        self.violations
            .push(format!("t={}.{:03}s {what}", t / 1000, t % 1000));
    }

    pub fn disrupt(&mut self) {
        self.disrupted_ms = Some(self.now_ms());
    }

    /// The latest hop record for `target`.
    pub fn record_for(&mut self, target: u8) -> Option<usize> {
        self.hops.iter().rposition(|h| h.target == target)
    }

    /// Note a drone announce for `target`, carrying `delay_ms` to its flip, in
    /// the hop records. One hop's announces count down to one flip; a fresh
    /// countdown is the next hop, even to the same target.
    pub fn note_announce(&mut self, target: u8, delay_ms: u64) {
        let now = self.now_ms();
        if let Some(last) = self.hops.last_mut() {
            if last.target == target
                && delay_ms <= last.last_delay_ms
                && now.saturating_sub(last.last_ms) < 1000
            {
                last.last_ms = now;
                last.last_delay_ms = delay_ms;
                return;
            }
        }
        let own_channel = target == self.drone.channel;
        if own_channel {
            self.stats.own_channel_announces += 1;
        } else {
            self.stats.hops_announced += 1;
        }
        self.hops.push(HopRecord {
            target,
            first_ms: now,
            last_ms: now,
            last_delay_ms: delay_ms,
            flip_ms: now + delay_ms,
            executed: false,
            own_channel,
            acked_by: BTreeSet::new(),
        });
        let kind = if own_channel { "own channel" } else { "hop" };
        self.log(format!(
            "drone announces {kind} -> {target} (gen {})",
            self.drone.generation
        ));
    }
}

/// A sender or receiver on the air.
#[derive(Clone, Copy, Debug)]
pub enum Node {
    Drone,
    Ground(usize),
}

/// Put `pkt` on the air from `from`, on the channel it is tuned to now. Each
/// receiver gets it after a drawn delay, maybe twice, maybe never, and only if
/// it is tuned to that channel when it lands.
pub fn transmit(w: &Shared, g: &mut World, from: Node, pkt: &[u8]) {
    let channel = match from {
        Node::Drone if g.drone.alive => g.drone.channel,
        Node::Drone => 0,
        Node::Ground(i) => g.grounds[i].channel,
    };
    if channel == 0 || g.outage {
        return;
    }
    let receivers: Vec<Node> = match from {
        Node::Drone => (0..g.grounds.len()).map(Node::Ground).collect(),
        Node::Ground(_) => vec![Node::Drone],
    };
    let is_ack = matches!(from, Node::Ground(_)) && pkt.len() == 51;
    for to in receivers {
        // The quiet tail is lossless: a late double beacon loss there would
        // start a legitimate return home with no time left to settle.
        let mut loss = if g.faults_on { g.f.loss } else { 0.0 };
        if is_ack && g.faults_on {
            loss += g.f.ack_loss;
        }
        let dup = g.f.dup;
        let copies = if g.faults_on && g.rng.chance(dup) {
            2
        } else {
            1
        };
        for _ in 0..copies {
            if g.rng.chance(loss) {
                if is_ack {
                    g.stats.lost_acks += 1;
                }
                continue;
            }
            let (lo, hi) = g.f.delay_ms;
            let at = Instant::now() + Duration::from_millis(g.rng.ms(lo, hi));
            let w = w.clone();
            let pkt = pkt.to_vec();
            tokio::spawn(async move {
                tokio::time::sleep_until(at).await;
                deliver(&w, to, channel, pkt);
            });
        }
    }
}

/// Land a frame at `to` if it is listening on `channel`.
fn deliver(w: &Shared, to: Node, channel: u8, pkt: Vec<u8>) {
    let g = lock(w);
    if g.outage {
        return;
    }
    match to {
        Node::Drone => {
            if g.drone.alive && g.drone.channel == channel {
                if let Some(port) = &g.port {
                    let _ = port.inbox.send(pkt);
                }
            }
        }
        Node::Ground(i) => {
            if g.grounds[i].rx_alive && g.grounds[i].channel == channel {
                let _ = g.grounds[i].inbox.send(pkt);
            }
        }
    }
}

/// The hop target of a 51-byte hop frame.
pub fn hop_target(pkt: &[u8]) -> Option<u8> {
    parse_hop_announce(pkt, &key()).map(|(t, _)| t)
}
