//! The router under test: the real [`FcConnection`] and mission-mirror core,
//! wired as the service wires them (`daemon`, `mission_mirror::run`), on a
//! simulated clock. ADOS time follows a sim-clock feed the harness ticks;
//! steady time (FC heartbeat liveness) is Tokio's paused clock, which the
//! harness advances in step. No socket, no timer: the FC link is an
//! in-process writer and the bytes the harness hands to `ingest`.

use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context, Poll};

use ados_mavlink_router::clock::RouterClock;
use ados_mavlink_router::config::MavlinkConfig;
use ados_mavlink_router::connection::{ClientOrigin, FcConnection, LinkSession};
use ados_mavlink_router::mission_mirror::{wire, MirrorCore, VehiclePosition};
use ados_mavlink_router::param_cache::ParamCache;
use ados_mavlink_router::rx_frame::RxFrame;
use ados_mavlink_router::state::VehicleState;
use ados_protocol::mavlink::ardupilotmega::MavMessage;
use ados_protocol::simclock::wire::Tick;
use ados_protocol::simclock::{Clock, SimFeed};
use bytes::Bytes;
use serde_json::Value;
use tokio::io::AsyncWrite;
use tokio::sync::{broadcast, Mutex};

/// The router's own `(system, component)`: the config defaults.
pub const OURS: (u8, u8) = (1, 191);
/// The mission mirror's poll period (`mission_mirror::run`).
pub const MIRROR_MS: u64 = 200;

type Outbox = Arc<StdMutex<Vec<Vec<u8>>>>;

/// The FC writer: every write the router makes toward the FC, queued for
/// the link.
struct FcWriter(Outbox);

impl AsyncWrite for FcWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        d: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.0.lock().unwrap().push(d.to_vec());
        Poll::Ready(Ok(d.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

pub struct Router {
    pub fc: Arc<FcConnection>,
    pub state: Arc<Mutex<VehicleState>>,
    pub mirror: MirrorCore,
    clock: RouterClock,
    feed: Arc<SimFeed>,
    tick_seq: u32,
    session: Option<LinkSession>,
    outbox: Outbox,
    mirror_rx: broadcast::Receiver<RxFrame>,
    /// The fan-out a ground station's proxy reads.
    pub proxy_rx: broadcast::Receiver<RxFrame>,
    /// The fan-out as the harness observes it.
    pub obs_rx: broadcast::Receiver<RxFrame>,
    client_rx: broadcast::Receiver<Bytes>,
    _dir: tempfile::TempDir,
}

impl Router {
    /// A router as the service starts one, at sim time `now` ms.
    pub fn new(cfg: MavlinkConfig, now: u64) -> Self {
        let feed = SimFeed::new();
        let clock = RouterClock::new(Clock::Sim(feed.clone()));
        let dir = tempfile::tempdir().expect("a temp dir");
        let state = Arc::new(Mutex::new(VehicleState {
            clock: clock.clone(),
            ..VehicleState::default()
        }));
        let params = Arc::new(Mutex::new(ParamCache::new(dir.path().join("params.json"))));
        let fc = FcConnection::with_clock(cfg, state.clone(), params, clock.clone());
        let mirror = MirrorCore::new(fc.our_ids());
        let mut r = Self {
            mirror_rx: fc.subscribe(),
            proxy_rx: fc.subscribe(),
            obs_rx: fc.subscribe(),
            client_rx: fc.subscribe_client_writes(),
            fc,
            state,
            mirror,
            clock,
            feed,
            tick_seq: 0,
            session: None,
            outbox: Arc::default(),
            _dir: dir,
        };
        r.set_time(now);
        r
    }

    /// Feed ADOS time `now` ms (sim time).
    pub fn set_time(&mut self, now: u64) {
        self.tick_seq += 1;
        let tick = Tick {
            flags: 0,
            seq: self.tick_seq,
            sim_ns: now * 1_000_000,
            grant_ns: (now + 10) * 1_000_000,
            speed: 1.0,
        };
        self.feed.ingest(&tick, std::time::Instant::now());
    }

    /// The FC transport opened (as `FcConnection::run` opens one).
    pub async fn open(&mut self) {
        let writer = Box::pin(FcWriter(self.outbox.clone()));
        self.fc.begin_session(writer, "sim:fc", 115_200).await;
        self.session = Some(self.fc.link_session().await);
    }

    /// The FC transport closed (read EOF).
    pub async fn close(&mut self) {
        self.session = None;
        self.fc.end_session().await;
    }

    pub fn is_open(&self) -> bool {
        self.session.is_some()
    }

    /// One read off the FC link.
    pub async fn ingest(&mut self, chunk: &[u8]) {
        if let Some(s) = self.session.as_mut() {
            self.fc.ingest(s, chunk).await;
        }
    }

    /// Everything the router wrote toward the FC since the last call.
    pub fn take_writes(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut *self.outbox.lock().unwrap())
    }

    /// A ground station's write toward the FC, through the client path.
    pub async fn client_write(&self, data: &[u8]) -> bool {
        self.fc
            .send_client_bytes(data, ClientOrigin::Trusted, None)
            .await
    }

    /// The mirror task's frame arms: every fan-out frame and client write
    /// since the last call, in arrival order per lane; its download frames
    /// go out through the router's own send path.
    pub async fn mirror_frames(&mut self) {
        let now = self.clock.now();
        let mut out = Vec::new();
        while let Some(f) = recv(&mut self.mirror_rx) {
            for m in wire::frames(&f.bytes) {
                out.extend(self.mirror.on_fc_frame(&m, now));
            }
        }
        while let Some(b) = recv(&mut self.client_rx) {
            for m in wire::frames(&b) {
                self.mirror.on_client_frame(&m, now);
            }
        }
        self.send_own(out).await;
    }

    /// The mirror task's tick arm: timers, the published mission block.
    pub async fn mirror_tick(&mut self) {
        let now = self.clock.now();
        let (fw, position) = {
            let s = self.state.lock().await;
            (s.firmware, VehiclePosition::of(&s))
        };
        self.mirror.set_firmware(fw);
        self.mirror.set_position(position);
        let out = self.mirror.tick(now);
        let mission = self.mirror.state_json(now);
        self.state.lock().await.mission = Some(mission);
        self.send_own(out).await;
    }

    async fn send_own(&self, out: Vec<MavMessage>) {
        for msg in out {
            self.fc.send_own(&msg).await;
        }
    }

    /// The published `mission` block.
    pub async fn mission_json(&self) -> Value {
        self.state
            .lock()
            .await
            .mission
            .clone()
            .unwrap_or(Value::Null)
    }
}

/// The next frame on a fan-out lane; a lagged reader skips ahead, as the
/// service's readers do.
pub fn recv<T: Clone>(rx: &mut broadcast::Receiver<T>) -> Option<T> {
    loop {
        match rx.try_recv() {
            Ok(v) => return Some(v),
            Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
            Err(_) => return None,
        }
    }
}
