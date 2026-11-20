//! The box: the production uplink ([`Uplink`] and its ops through the command
//! socket's [`dispatch_on`]), a receive generation ([`GenTx`]) driving a fake
//! transmitter pair, the presence watcher's holder timer, and the LAN route's
//! own admission ([`admit_phone`], [`ReplayGuard`], [`phone_command`]) and
//! take-back call ([`sign::yield_request`], [`sign::YIELD_TIMEOUT`]). Every
//! piece runs as a task on the simulation's paused clock; what they do that
//! the world must see comes back on one channel ([`Out`]).

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ados_control::routes::ground_station_uplink::phones::{self, ApprovedPhone};
use ados_control::routes::ground_station_uplink::sign::{self, ReplayGuard};
use ados_control::routes::ground_station_uplink::{
    admit_phone, phone_command, take_back_command, UplinkRequest,
};
use ados_groundlink::cmdsock::uplink_ops::dispatch_on;
use ados_groundlink::daemon::uplink_tx::{GenTx, TxPair};
use ados_groundlink::uplink::ops::{self, TakeContext};
use ados_groundlink::uplink::watcher::{classify, GroundFrame};
use ados_groundlink::uplink::{Uplink, UplinkState};
use ados_groundlink::wfb_rx::TxStdout;
use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::net::{Kind, Signed, Via};
use crate::rng::Rng;

/// The box's ground device id, the one its beacons and take-back calls carry.
pub const BOX_ID: &str = "gs-box-01";

/// The unix milliseconds the simulation's wall clock starts at.
pub const WALL0: i64 = 1_790_000_000_000;

/// How often the presence watcher checks the holder timer
/// (`uplink::watcher`'s tick).
const WATCH_TICK: Duration = Duration::from_secs(1);

/// The command socket's take context on a standalone box with no bind.
fn direct() -> TakeContext {
    TakeContext {
        bind_active: false,
        direct_role: true,
    }
}

/// The box's wall clock: `WALL0` plus the simulation's elapsed time.
pub fn wall_ms(t0: Instant) -> i64 {
    WALL0 + (Instant::now() - t0).as_millis() as i64
}

/// What the box's tasks report to the world.
#[derive(Debug)]
pub enum Out {
    /// The route's reply to a copy of a phone's request.
    Reply {
        phone: Option<usize>,
        msg: u64,
        via: Via,
        reply: Value,
    },
    /// The route admitted this copy: its MAC verified and the replay guard
    /// recorded its timestamp.
    Admitted {
        msg: u64,
        via: Via,
        ts: i64,
        take: Option<(String, String)>,
    },
    /// The route refused this copy before it reached the uplink.
    Refused { msg: u64, via: Via, why: String },
    /// A take-back call to the holder at `url`; the box waits on `answer`.
    Yield {
        url: String,
        ts: i64,
        body: String,
        mac: String,
        answer: oneshot::Sender<bool>,
    },
    /// The box is about to transmit without the holder's confirmation that it
    /// stopped (an unanswered take-back, or the holder timer).
    Revoke { holder: String, why: &'static str },
    /// The holder timer took the uplink back from `holder`.
    FellBack { holder: String },
    /// The operator's take-back finished; `changed` when a phone took the
    /// uplink after the route read the holder, and the op refused.
    TookBack { confirmed: bool, changed: bool },
}

/// The box's ground transmitter pair on air: up from the spawn until both
/// processes exited.
struct FakeTx {
    air: Arc<AtomicBool>,
    rng: Rng,
}

impl TxPair for FakeTx {
    fn is_running(&self) -> bool {
        self.air.load(Ordering::SeqCst)
    }

    async fn start(&mut self) -> std::io::Result<TxStdout> {
        self.air.store(true, Ordering::SeqCst);
        Ok(TxStdout {
            control: None,
            aux: None,
        })
    }

    /// `SIGTERM`, then up to 2 s before `SIGKILL`: both radiate until gone.
    async fn stop(&mut self) {
        tokio::time::sleep(Duration::from_millis(self.rng.ms(100, 2_100))).await;
        self.air.store(false, Ordering::SeqCst);
    }
}

impl Drop for FakeTx {
    /// The generation ended: dropping the pair killpg's both.
    fn drop(&mut self) {
        self.air.store(false, Ordering::SeqCst);
    }
}

/// The standby watcher's receiver: listening while alive.
struct FakeWatcher(Arc<AtomicBool>);

impl Drop for FakeWatcher {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// The box and its running tasks.
pub struct BoxSide {
    pub u: Arc<Uplink>,
    /// Both ground transmitters radiate.
    pub air: Arc<AtomicBool>,
    /// The standby watcher's receiver runs.
    pub listening: Arc<AtomicBool>,
    guard: Arc<Mutex<ReplayGuard>>,
    approved: Arc<Vec<ApprovedPhone>>,
    out: mpsc::UnboundedSender<Out>,
    t0: Instant,
    dir: PathBuf,
    tasks: Vec<JoinHandle<()>>,
}

impl BoxSide {
    /// Boot the box: transmitting, as a box always starts.
    pub fn boot(
        dir: &Path,
        approved: Arc<Vec<ApprovedPhone>>,
        out: mpsc::UnboundedSender<Out>,
        t0: Instant,
        rng: &mut Rng,
    ) -> Self {
        let mut b = Self {
            u: Arc::new(Uplink::with_sidecar(dir.join("wfb-stats.json"))),
            air: Arc::new(AtomicBool::new(false)),
            listening: Arc::new(AtomicBool::new(false)),
            guard: Arc::new(Mutex::new(ReplayGuard::load(Self::replay_path(dir)))),
            approved,
            out,
            t0,
            dir: dir.to_path_buf(),
            tasks: Vec::new(),
        };
        b.spawn_service(rng);
        b
    }

    fn replay_path(dir: &Path) -> PathBuf {
        dir.join("uplink-replay.json")
    }

    /// The receive generation and the presence watcher's holder timer.
    fn spawn_service(&mut self, rng: &mut Rng) {
        let (u, air, listening) = (self.u.clone(), self.air.clone(), self.listening.clone());
        let tx = FakeTx {
            air,
            rng: rng.fork(),
        };
        self.tasks.push(tokio::spawn(async move {
            let spawn_watcher = || {
                listening.store(true, Ordering::SeqCst);
                Ok(FakeWatcher(listening.clone()))
            };
            let (mut gen_tx, mut asks) = GenTx::attach(&u, tx);
            if !gen_tx.apply(asks.first(), spawn_watcher).await {
                return;
            }
            while let Some(ask) = asks.next().await {
                if !gen_tx.apply(ask, spawn_watcher).await {
                    break;
                }
            }
            gen_tx.close().await;
        }));
        let (u, out) = (self.u.clone(), self.out.clone());
        self.tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(WATCH_TICK);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let now = Instant::now();
                let Some(h) = u.holder().filter(|_| u.holder_absent(now)) else {
                    continue;
                };
                let _ = out.send(Out::Revoke {
                    holder: h.device_id.clone(),
                    why: "holder_absent",
                });
                if ops::fall_back_if_absent(&u, now).await {
                    let _ = out.send(Out::FellBack {
                        holder: h.device_id,
                    });
                }
            }
        }));
    }

    /// The radio service restarts: every task ends (a generation that ends
    /// kills its pair), and the box boots transmitting with a fresh uplink.
    pub async fn restart(&mut self, rng: &mut Rng) {
        for t in self.tasks.drain(..) {
            t.abort();
            let _ = t.await;
        }
        self.u = Arc::new(Uplink::with_sidecar(self.dir.join("wfb-stats.json")));
        self.restart_control();
        self.spawn_service(rng);
    }

    /// The control service restarts: its replay guard reloads from disk.
    pub fn restart_control(&mut self) {
        *self.guard.lock().unwrap() = ReplayGuard::load(Self::replay_path(&self.dir));
    }

    pub fn standby_holder(&self) -> Option<(String, String)> {
        match self.u.state() {
            UplinkState::Standby(h) => Some((h.device_id, h.phone_key)),
            UplinkState::Transmitting => None,
        }
    }

    /// A beacon frame reaching the box: heard only while the standby watcher's
    /// receiver runs. `Some(true)` when it was the holder's.
    pub fn hear(&self, frame: &[u8; 68], pair_key: &[u8; 32]) -> Option<bool> {
        if !self.listening.load(Ordering::SeqCst) {
            return None;
        }
        match classify(frame, pair_key) {
            GroundFrame::Beacon(id) => Some(self.u.note_beacon(&id, Instant::now())),
            _ => None,
        }
    }

    /// A copy of a signed message reaching `POST /api/v1/ground-station/uplink`
    /// from `peer`.
    pub fn request(&mut self, from: Option<usize>, peer: IpAddr, via: Via, req: Signed) {
        self.tasks.retain(|t| !t.is_finished());
        let (u, guard, approved, out, t0) = (
            self.u.clone(),
            self.guard.clone(),
            self.approved.clone(),
            self.out.clone(),
            self.t0,
        );
        self.tasks.push(tokio::spawn(async move {
            let msg = req.msg;
            let reply = |v: Value| {
                let _ = out.send(Out::Reply {
                    phone: from,
                    msg,
                    via,
                    reply: v,
                });
            };
            let refuse = |why: String| {
                reply(json!({"ok": false, "error": why}));
                let _ = out.send(Out::Refused { msg, via, why });
            };
            let parsed = serde_json::from_slice::<UplinkRequest>(&req.body).ok();
            let Some(body) = parsed.filter(|r| r.op == "take" || r.op == "give_back") else {
                return refuse("E_BAD_REQUEST".into());
            };
            let mut headers = HeaderMap::new();
            let mut put = |name: &str, value: &str| {
                if let (Ok(n), Ok(v)) = (
                    HeaderName::from_bytes(name.as_bytes()),
                    HeaderValue::from_str(value),
                ) {
                    headers.insert(n, v);
                }
            };
            if let Some(p) = &req.phone_pub {
                put("X-ADOS-Phone-Key", p);
            }
            put("X-ADOS-Uplink-Ts", &req.ts.to_string());
            put("X-ADOS-Uplink-Mac", &req.mac);
            let admitted = {
                let mut g = guard.lock().unwrap();
                admit_phone(&approved, &headers, &req.body, wall_ms(t0), &mut g)
            };
            let fingerprint = match admitted {
                Ok(f) => f,
                Err(e) => return refuse(format!("{}:{e:?}", e.code())),
            };
            let take = (body.op == "take").then(|| {
                let id = body.holder_device_id.clone().unwrap_or_default();
                (id, fingerprint.clone())
            });
            let _ = out.send(Out::Admitted {
                msg,
                via,
                ts: req.ts,
                take,
            });
            let line = phone_command(&body, &fingerprint, Some(peer)).to_string();
            reply(
                dispatch_on(&u, line.as_bytes(), direct)
                    .await
                    .unwrap_or(Value::Null),
            );
        }));
    }

    /// The box's own operator takes the uplink back: the route's transition
    /// 3. Tell the holder first, wait up to [`sign::YIELD_TIMEOUT`] for its
    /// answer, then transmit either way.
    pub fn take_back(&mut self) {
        self.tasks.retain(|t| !t.is_finished());
        let (u, approved, out, t0) = (
            self.u.clone(),
            self.approved.clone(),
            self.out.clone(),
            self.t0,
        );
        self.tasks.push(tokio::spawn(async move {
            let status = dispatch_on(&u, br#"{"op":"uplink_status"}"#, direct)
                .await
                .unwrap_or(Value::Null);
            let mut confirmed = false;
            if status["state"] == json!("standby") {
                let url = status["yield_url"].as_str();
                let key = status["holder_phone_key"].as_str().and_then(|f| {
                    phones::by_fingerprint(&approved, f).and_then(ApprovedPhone::uplink_key)
                });
                if let (Some(url), Some(key)) = (url, key) {
                    let ts = wall_ms(t0);
                    let (body, mac) = sign::yield_request(&key, BOX_ID, ts);
                    let (tx, rx) = oneshot::channel();
                    let _ = out.send(Out::Yield {
                        url: url.to_string(),
                        ts,
                        body,
                        mac,
                        answer: tx,
                    });
                    confirmed = matches!(
                        tokio::time::timeout(sign::YIELD_TIMEOUT, rx).await,
                        Ok(Ok(true))
                    );
                }
                if let (false, Some(holder)) =
                    (confirmed, status["uplink"]["holder_device_id"].as_str())
                {
                    let _ = out.send(Out::Revoke {
                        holder: holder.to_string(),
                        why: "take_back_unconfirmed",
                    });
                }
            }
            let line = take_back_command(&status, confirmed).to_string();
            let reply = dispatch_on(&u, line.as_bytes(), direct)
                .await
                .unwrap_or(Value::Null);
            let _ = out.send(Out::TookBack {
                confirmed,
                changed: reply["error"] == json!("E_UPLINK_CHANGED"),
            });
        }));
    }
}

/// A signed message the box sent, as it leaves on the LAN.
pub fn yield_signed(msg: u64, ts: i64, body: String, mac: String) -> Signed {
    Signed {
        msg,
        kind: Kind::Yield,
        ts,
        mac,
        phone_pub: None,
        body: body.into_bytes(),
    }
}
