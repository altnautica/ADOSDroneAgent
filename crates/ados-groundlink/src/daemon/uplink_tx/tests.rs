use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::*;
use crate::uplink::ops::{self, TakeContext, TakeRequest};
use crate::uplink::{Uplink, UplinkState, TX_SWITCH_DEADLINE};

/// What the fake transmitters and the fake watcher receiver did, in order.
type Log = Arc<Mutex<Vec<&'static str>>>;

fn new_uplink(dir: &tempfile::TempDir) -> Arc<Uplink> {
    Arc::new(Uplink::with_sidecar(dir.path().join("wfb-stats.json")))
}

fn events(log: &Log) -> Vec<&'static str> {
    log.lock().unwrap().clone()
}

/// Two ground transmitters that spawn nothing. A start may be held at
/// `gate` until the test releases it; a stop takes as long as `wfb_tx` takes
/// to exit on `SIGTERM`.
struct FakeTx {
    running: bool,
    log: Log,
    /// Notified when a start begins.
    entered: Arc<Notify>,
    /// When set, the next start waits here.
    gate: Option<Arc<Notify>>,
    /// Every start fails, as a missing `wfb_tx` would.
    fail: bool,
}

impl FakeTx {
    fn new(log: &Log) -> Self {
        Self {
            running: false,
            log: log.clone(),
            entered: Arc::new(Notify::new()),
            gate: None,
            fail: false,
        }
    }
}

impl TxPair for FakeTx {
    fn is_running(&self) -> bool {
        self.running
    }

    async fn start(&mut self) -> std::io::Result<TxStdout> {
        self.entered.notify_one();
        if let Some(gate) = self.gate.take() {
            gate.notified().await;
        }
        if self.fail {
            return Err(std::io::Error::other("wfb_tx is missing"));
        }
        self.running = true;
        self.log.lock().unwrap().push("start");
        Ok(TxStdout {
            control: None,
            aux: None,
        })
    }

    async fn stop(&mut self) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        self.running = false;
        self.log.lock().unwrap().push("stop");
    }
}

/// The standby watcher's receiver; logs its spawn and its end.
struct FakeWatcher(Log);

impl Drop for FakeWatcher {
    fn drop(&mut self) {
        self.0.lock().unwrap().push("watcher_down");
    }
}

fn watcher(log: &Log) -> impl FnOnce() -> std::io::Result<FakeWatcher> {
    let log = log.clone();
    move || {
        log.lock().unwrap().push("watcher_up");
        Ok(FakeWatcher(log))
    }
}

/// A receive generation as `receive_loop` runs one: attach, carry out the ask
/// in force, then every later ask in place, and close when an ask fails.
fn run_generation(u: &Arc<Uplink>, tx: FakeTx, log: &Log) -> JoinHandle<()> {
    let (u, log) = (u.clone(), log.clone());
    tokio::spawn(async move {
        let (mut gen_tx, mut asks) = GenTx::attach(&u, tx);
        if !gen_tx.apply(asks.first(), watcher(&log)).await {
            return;
        }
        while let Some(ask) = asks.next().await {
            if !gen_tx.apply(ask, watcher(&log)).await {
                break;
            }
        }
        gen_tx.close().await;
    })
}

const OK_CTX: TakeContext = TakeContext {
    bind_active: false,
    direct_role: true,
};

fn take_req() -> TakeRequest {
    TakeRequest {
        holder_device_id: "a1b2c3d4e5f6".into(),
        holder_name: "Ops phone".into(),
        phone_key: "KEY-1".into(),
        yield_url: None,
    }
}

/// A take whose reply is logged the moment it is made.
fn spawn_take(u: &Arc<Uplink>, log: &Log) -> JoinHandle<serde_json::Value> {
    let (u, log) = (u.clone(), log.clone());
    tokio::spawn(async move {
        let reply = ops::take(&u, take_req(), OK_CTX).await;
        log.lock().unwrap().push("standby_reply");
        reply
    })
}

/// The start race: the generation read "transmit" and is spawning both
/// transmitters when a take asks for standby. The take must not reply until
/// the generation carried out the stop and both transmitters exited.
#[tokio::test(start_paused = true)]
async fn a_take_during_the_generations_start_replies_only_after_the_stop() {
    let dir = tempfile::tempdir().unwrap();
    let u = new_uplink(&dir);
    let log = Log::default();
    let gate = Arc::new(Notify::new());
    let mut tx = FakeTx::new(&log);
    tx.gate = Some(gate.clone());
    let entered = tx.entered.clone();
    run_generation(&u, tx, &log);
    entered.notified().await;

    let take = spawn_take(&u, &log);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        !take.is_finished(),
        "the take replied while the transmitters were starting"
    );
    gate.notify_one();
    let reply = take.await.unwrap();
    assert_eq!(reply["state"], json!("standby"));
    let tx_events: Vec<_> = events(&log)
        .into_iter()
        .filter(|e| !e.starts_with("watcher"))
        .collect();
    assert_eq!(tx_events, ["start", "stop", "standby_reply"]);
    assert!(!u.transmitters_up());
}

/// One generation follows every change of the ask without ending: standby
/// stops the pair and starts the watcher receiver, transmitting drops the
/// watcher and starts the pair again.
#[tokio::test(start_paused = true)]
async fn a_change_of_the_ask_is_applied_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let u = new_uplink(&dir);
    let log = Log::default();
    let generation = run_generation(&u, FakeTx::new(&log), &log);

    assert!(u.start_transmitters(TX_SWITCH_DEADLINE).await);
    assert!(u.transmitters_up());
    assert!(u.stop_transmitters(TX_SWITCH_DEADLINE).await);
    assert!(!u.transmitters_up());
    // Standby asked again: nothing restarts, no second watcher.
    assert!(u.stop_transmitters(TX_SWITCH_DEADLINE).await);
    assert!(u.start_transmitters(TX_SWITCH_DEADLINE).await);
    assert_eq!(
        events(&log),
        ["start", "stop", "watcher_up", "watcher_down", "start"]
    );
    assert!(!generation.is_finished(), "the generation went on");
}

/// A generation that attaches while the box stands by never starts the pair;
/// it runs only the watcher receiver, and reports so.
#[tokio::test(start_paused = true)]
async fn a_generation_attached_in_standby_runs_only_the_watcher() {
    let dir = tempfile::tempdir().unwrap();
    let u = new_uplink(&dir);
    let log = Log::default();
    // No generation yet: the stop holds at once, and the next one reads it.
    assert!(u.stop_transmitters(TX_SWITCH_DEADLINE).await);
    run_generation(&u, FakeTx::new(&log), &log);
    tokio::time::sleep(Duration::from_millis(10)).await;
    // Asked again once it runs: it reports standby without starting anything.
    assert!(u.stop_transmitters(TX_SWITCH_DEADLINE).await);
    assert_eq!(events(&log), ["watcher_up"]);
    assert!(!u.transmitters_up());
}

/// A watcher receiver that cannot start does not end the generation: the
/// holder timer still runs and falls back after the stale window.
#[tokio::test(start_paused = true)]
async fn a_watcher_that_cannot_start_does_not_end_the_generation() {
    let dir = tempfile::tempdir().unwrap();
    let u = new_uplink(&dir);
    let log = Log::default();
    let (mut gen_tx, asks) = GenTx::<_, FakeWatcher>::attach(&u, FakeTx::new(&log));
    assert!(gen_tx.apply(asks.first(), watcher(&log)).await);
    let standby = TxAsk {
        transmit: false,
        version: asks.first().version + 1,
    };
    assert!(
        gen_tx
            .apply(standby, || Err(std::io::Error::other("no adapter")))
            .await
    );
    assert_eq!(events(&log), ["start", "stop"]);
}

/// A pair that cannot start ends the generation, which detaches: a take then
/// stands by at once, and a give-back waits for the next generation.
#[tokio::test(start_paused = true)]
async fn a_pair_that_cannot_start_ends_the_generation_and_detaches_it() {
    let dir = tempfile::tempdir().unwrap();
    let u = new_uplink(&dir);
    let log = Log::default();
    let mut tx = FakeTx::new(&log);
    tx.fail = true;
    run_generation(&u, tx, &log).await.unwrap();
    assert!(u.stop_transmitters(TX_SWITCH_DEADLINE).await);
    assert!(!u.start_transmitters(Duration::from_secs(1)).await);
    assert!(events(&log).is_empty());
}

/// A generation that ends (a stalled transmitter, the receiver exiting) while
/// a take waits on it stops the pair and waits for both to exit before it
/// detaches, so the take still replies only once nothing transmits.
#[tokio::test(start_paused = true)]
async fn closing_a_generation_stops_the_pair_before_it_detaches() {
    let dir = tempfile::tempdir().unwrap();
    let u = new_uplink(&dir);
    let log = Log::default();
    let (mut gen_tx, asks) = GenTx::attach(&u, FakeTx::new(&log));
    assert!(gen_tx.apply(asks.first(), watcher(&log)).await);
    let take = spawn_take(&u, &log);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!take.is_finished());
    gen_tx.close().await;
    let reply = take.await.unwrap();
    assert_eq!(reply["state"], json!("standby"));
    assert_eq!(events(&log), ["start", "stop", "standby_reply"]);
    assert!(matches!(u.state(), UplinkState::Standby(_)));
}
