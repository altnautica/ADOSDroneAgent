//! The drone: the real hop supervisor ([`run_hop_loop`]) and presence beacon
//! emitter over a simulated radio ([`SimRadio`]), driven by a model of the
//! radio service's respawn loop (`daemon/service.rs`): bring a radio group up
//! on [`respawn_channel`], watch it at the 1 s exit-poll cadence, abort the
//! group's tasks when a plane exits, wait the 2 s respawn delay, bring it up
//! again.

use std::collections::BTreeSet;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use ados_protocol::shutdown::Shutdown;
use ados_radio::channel::{pick_from_ranking, standard_channels};
use ados_radio::config::WfbConfig;
use ados_radio::daemon::hop_supervisor::{
    async_trait, emit_presence_beacons_on, run_hop_loop, ControlPlane, HopContext, HopRadio,
};
use ados_radio::hop::{hop_announce_delay_ms, respawn_channel, HopState};
use ados_radio::link_proof::RxProof;
use ados_radio::link_quality::LinkStats;
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use tokio::time::{sleep, Duration, Instant};

use crate::world::{hop_target, key, lock, transmit, CrashSnap, Node, ReturnPort, Shared, BAND};

/// The drone's device-id.
pub const DRONE_ID: &str = "drone-sim-0001";
/// The production respawn delay (`service.rs` `RESPAWN_DELAY`).
const RESPAWN_DELAY: Duration = Duration::from_secs(2);

/// One radio-group generation's view of the drone radio.
pub struct SimRadio {
    pub w: Shared,
    pub generation: u64,
}

/// A bound return port. Dropping it releases the port, as closing the
/// socket does.
pub struct SimPort {
    w: Shared,
    generation: u64,
    rx: AsyncMutex<mpsc::UnboundedReceiver<Vec<u8>>>,
}

impl Drop for SimPort {
    fn drop(&mut self) {
        let mut g = lock(&self.w);
        if g.port
            .as_ref()
            .is_some_and(|p| p.generation == self.generation)
        {
            g.port = None;
            let gen = self.generation;
            g.log(format!("drone return port released (gen {gen})"));
        }
    }
}

#[async_trait]
impl ControlPlane for SimRadio {
    type Port = SimPort;

    fn iface(&self) -> &str {
        "sim0"
    }

    async fn bind_return_port(&self) -> io::Result<SimPort> {
        let mut g = lock(&self.w);
        if let Some(p) = &g.port {
            let holder = p.generation;
            g.log(format!(
                "drone return port busy (gen {} binding, gen {holder} holds it)",
                self.generation
            ));
            return Err(io::Error::from(io::ErrorKind::AddrInUse));
        }
        let (tx, rx) = mpsc::unbounded_channel();
        g.port = Some(ReturnPort {
            generation: self.generation,
            inbox: tx,
        });
        g.log(format!("drone return port bound (gen {})", self.generation));
        Ok(SimPort {
            w: self.w.clone(),
            generation: self.generation,
            rx: AsyncMutex::new(rx),
        })
    }

    async fn recv_return(&self, port: &SimPort, buf: &mut [u8]) -> io::Result<usize> {
        let pkt = port.rx.lock().await.recv().await;
        match pkt {
            Some(p) => {
                let n = p.len().min(buf.len());
                buf[..n].copy_from_slice(&p[..n]);
                Ok(n)
            }
            None => std::future::pending().await,
        }
    }

    async fn send_control(&self, pkt: &[u8]) {
        let mut g = lock(&self.w);
        if pkt.len() == 51 && g.drone.alive {
            if let Some(t) = hop_target(pkt) {
                g.note_announce(t, hop_announce_delay_ms(pkt).unwrap_or(0));
            }
        }
        transmit(&self.w, &mut g, Node::Drone, pkt);
    }

    async fn live_channel(&self) -> Option<u8> {
        let ch = lock(&self.w).drone.channel;
        (ch != 0).then_some(ch)
    }
}

#[async_trait]
impl HopRadio for SimRadio {
    async fn set_channel(&self, channel: u8) -> bool {
        let (fail, took, prev) = {
            let mut g = lock(&self.w);
            let now = g.now_ms();
            let announced = g
                .hops
                .last()
                .filter(|h| h.target == channel && now.saturating_sub(h.last_ms) < 4000)
                .is_some();
            if announced {
                if let Some(h) = g.hops.last_mut() {
                    if !h.executed {
                        h.executed = true;
                    }
                }
                g.stats.hops_executed += 1;
            } else {
                // Not the flip of an announced hop: the return home, which
                // moves the drone on its own; a hop check spanning it is moot.
                g.stats.return_homes += 1;
                g.disrupt();
            }
            if g.bind_open {
                let what = if announced { "hop flip" } else { "return home" };
                g.violate(format!(
                    "{what} to {channel} retuned the radio while a bind owns the adapter"
                ));
            }
            let p = g.f.drone_retune_fail;
            let fail = g.faults_on && g.rng.chance(p);
            let took = g.rng.ms(100, 300);
            let prev = g.drone.channel;
            g.drone.channel = 0;
            g.log(format!("drone set_channel {channel} (from {prev})"));
            (fail, took, prev)
        };
        sleep(Duration::from_millis(took)).await;
        let mut g = lock(&self.w);
        g.drone.channel = if fail { prev } else { channel };
        if fail {
            g.stats.drone_retune_fails += 1;
            g.drone_failed_ms = Some(g.now_ms());
            g.log(format!(
                "drone set_channel {channel} FAILED, still on {prev}"
            ));
        }
        !fail
    }

    async fn respawn_group(&self) -> bool {
        let took = {
            let mut g = lock(&self.w);
            g.drone.alive = false;
            g.drone.respawning = true;
            g.log("drone respawn_group");
            g.rng.ms(200, 600)
        };
        sleep(Duration::from_millis(took)).await;
        let mut g = lock(&self.w);
        let p = g.f.drone_spawn_fail;
        let ok = !(g.faults_on && g.rng.chance(p));
        g.drone.alive = ok;
        g.drone.respawning = false;
        if !ok {
            g.log("drone respawn_group FAILED");
            g.drone_failed_ms = Some(g.now_ms());
            g.disrupt();
        }
        ok
    }

    async fn pick_hop_target(&self, current: u8, enabled: Option<&BTreeSet<u8>>) -> u8 {
        let took = {
            let mut g = lock(&self.w);
            if g.bind_open {
                g.violate("a hop was started while a bind owns the adapter".to_string());
            }
            g.rng.ms(500, 2500)
        };
        // The live scan.
        sleep(Duration::from_millis(took)).await;
        let mut g = lock(&self.w);
        let bad = g.bad;
        let mut ranked: Vec<(u8, u32)> = standard_channels()
            .iter()
            .map(|&(c, _)| {
                let aps = if Some(c) == bad {
                    6
                } else {
                    (g.rng.next_u64() % 3) as u32
                };
                (c, aps)
            })
            .collect();
        ranked.sort_by_key(|&(_, n)| n);
        pick_from_ranking(&ranked, current, "u-nii-3", enabled)
    }

    async fn restore_monitor_if_needed(&self, _current: u8) {}

    fn bind_active(&self) -> bool {
        lock(&self.w).bind_open
    }

    fn paired(&self) -> bool {
        true
    }

    fn write_sidecar(&self, _name: &str, _value: &serde_json::Value) {}
}

/// What the drone's radio service keeps for its lifetime.
pub struct Drone {
    pub state: Arc<AsyncMutex<HopState>>,
    pub link: Arc<AsyncMutex<LinkStats>>,
    pub cfg: WfbConfig,
}

impl Drone {
    pub fn new(periodic: bool) -> Self {
        let cfg = WfbConfig {
            periodic_hop_enabled: periodic,
            hop_period_seconds: 20,
            ..WfbConfig::default()
        };
        Self {
            state: Arc::new(AsyncMutex::new(HopState::new(cfg.rendezvous_channel()))),
            link: Arc::new(AsyncMutex::new(LinkStats::default())),
            cfg,
        }
    }
}

/// The radio service's respawn loop over the simulated radio.
pub async fn service(w: Shared, d: Arc<Drone>) {
    let restart_count = Arc::new(AtomicU64::new(0));
    let operating = Arc::new(AtomicU64::new(d.cfg.rendezvous_channel() as u64));
    loop {
        let took = lock(&w).rng.ms(600, 1500);
        sleep(Duration::from_millis(took)).await;
        // The bring-up's channel: the committed hop while the peer was heard
        // recently, else the rendezvous home.
        // `bring_up_group`: tune it (it lands here), then the record follows.
        let (channel, peer_fresh, committed) = {
            let mut s = d.state.lock().await;
            let now = Instant::now().into_std();
            let channel = respawn_channel(&s, now);
            let committed = s.channel;
            s.record_bring_up(channel);
            (channel, s.peer().is_some() && !s.peer_is_stale(), committed)
        };
        let (generation, manual_rx) = {
            let mut g = lock(&w);
            g.drone.generation += 1;
            g.drone.channel = channel;
            g.drone.alive = true;
            g.drone.respawning = false;
            g.drone.gen_started_ms = g.now_ms();
            g.stats.bringups += 1;
            g.disrupt();
            let (tx, rx) = mpsc::channel(8);
            g.manual_tx = Some(tx);
            let gen = g.drone.generation;
            g.log(format!(
                "drone bring-up gen {gen} on {channel} (committed {committed}, peer fresh {peer_fresh})"
            ));
            check_rejoin(&mut g, channel, peer_fresh);
            (gen, rx)
        };
        operating.store(channel as u64, Ordering::Relaxed);
        let radio = Arc::new(SimRadio {
            w: w.clone(),
            generation,
        });
        let ctx = HopContext {
            cfg: d.cfg.clone(),
            pair_key: key(),
            device_id: DRONE_ID.to_string(),
            link: d.link.clone(),
            enabled_channels: Arc::new(BAND.into_iter().collect()),
            restart_count: restart_count.clone(),
            rx_proof: RxProof::new(),
            proof_reference: Instant::now().into_std(),
            operating_channel: operating.clone(),
            state: d.state.clone(),
        };
        // The per-bring-up cancel: production fires it only on a service
        // shutdown and aborts the tasks on a respawn, as here.
        let cancel = Shutdown::new();
        let hop = tokio::spawn(run_hop_loop(radio.clone(), ctx, manual_rx, cancel.clone()));
        let beacon = {
            let radio = radio.clone();
            let home = d.cfg.rendezvous_channel();
            tokio::spawn(async move {
                emit_presence_beacons_on(&*radio, DRONE_ID, home, &key(), cancel).await
            })
        };
        // The group-exit watch: every plane's liveness at a 1 s cadence.
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.tick().await;
        loop {
            tick.tick().await;
            let g = lock(&w);
            if !g.drone.alive && !g.drone.respawning {
                break;
            }
        }
        hop.abort();
        beacon.abort();
        {
            let mut g = lock(&w);
            g.manual_tx = None;
            g.disrupt();
            g.log(format!("drone gen {generation} exited; respawn in 2 s"));
        }
        sleep(RESPAWN_DELAY).await;
    }
}

/// A bring-up after a crash rejoins every ground that was with the drone when
/// it crashed (and has not moved since), while the peer is fresh and no hop
/// was in flight.
fn check_rejoin(g: &mut crate::world::World, channel: u8, peer_fresh: bool) {
    let Some(snap) = g.crash.take() else {
        return;
    };
    let hop_in_flight = g
        .hops
        .last()
        .is_some_and(|h| snap.at_ms.saturating_sub(h.last_ms) < 10_000);
    if !peer_fresh || hop_in_flight {
        return;
    }
    for i in 0..g.grounds.len() {
        let was_with = snap.ground_channels[i] == snap.drone_channel;
        let still = g.grounds[i].channel == snap.ground_channels[i];
        if was_with && still {
            g.stats.rejoin_checks += 1;
            if channel != snap.drone_channel {
                let name = g.grounds[i].name;
                g.violate(format!(
                    "respawned radio came up on {channel}, not on {} where it and {name} were",
                    snap.drone_channel
                ));
            }
        }
    }
}

/// Crash the drone's wfb process group now and then.
pub async fn crasher(w: Shared) {
    let Some(every) = lock(&w).f.crash_every_s else {
        return;
    };
    loop {
        let gap = lock(&w).rng.secs(every);
        sleep(Duration::from_millis(gap)).await;
        let mut g = lock(&w);
        if !g.faults_on {
            return;
        }
        if g.drone.alive && !g.drone.respawning && g.drone.channel != 0 {
            g.drone.alive = false;
            g.stats.crashes += 1;
            g.disrupt();
            g.crash = Some(CrashSnap {
                at_ms: g.now_ms(),
                drone_channel: g.drone.channel,
                ground_channels: g.grounds.iter().map(|r| r.channel).collect(),
            });
            g.log("drone wfb plane CRASHED");
        }
    }
}

/// The uplink stats the reactive trigger reads: real packets while a ground
/// is on the drone's channel, with the loss the channel's interference makes.
pub async fn link_feed(w: Shared, d: Arc<Drone>) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        let stats = {
            let g = lock(&w);
            let ch = g.drone.channel;
            let up =
                g.drone.alive && ch != 0 && !g.outage && g.grounds.iter().any(|r| r.channel == ch);
            if up {
                LinkStats {
                    timestamp: "sim".to_string(),
                    packets_received: 500,
                    loss_percent: if g.bad == Some(ch) { 25.0 } else { 1.0 },
                    rssi_dbm: -55.0,
                    ..LinkStats::default()
                }
            } else {
                LinkStats::default()
            }
        };
        *d.link.lock().await = stats;
    }
}
