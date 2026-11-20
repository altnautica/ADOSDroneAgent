//! The simulated host under one rig's supervisor process: the
//! [`BindHost`] and [`ProcessManager`] the real orchestrator drives. Each
//! handle belongs to one process generation; once that process is killed,
//! every call it makes blocks forever, as a dead process makes no more calls.
//! A `systemctl` job already handed to the service manager still takes effect.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ados_protocol::logd::emitter::EventEmitter;
use ados_supervisor::bind::keys::{apply_fleet_fields, apply_pair_fields, FleetIdentity};
use ados_supervisor::bind::orchestrator::{BindHost, InjectionPrepOutcome, SocatFailure};
use ados_supervisor::bind::{BindRole, TUNNEL_POLL_INTERVAL, UPSTREAM_DRONE_KEY, UPSTREAM_GS_KEY};
use ados_supervisor::process_manager::ProcessManager;
use async_trait::async_trait;

use crate::world::{key_blob, lock, File, Shared};

/// One rig's host, as seen by one supervisor process generation.
#[derive(Clone)]
pub struct SimHost {
    pub w: Shared,
    pub rig: usize,
    pub epoch: u32,
}

impl SimHost {
    fn alive(&self) -> bool {
        lock(&self.w).rigs[self.rig].epoch == self.epoch
    }

    /// Block forever if this handle's process is dead.
    async fn gate(&self) {
        if !self.alive() {
            std::future::pending::<()>().await;
        }
    }

    /// Take a seeded time from `range`, then return if still alive.
    async fn take(&self, range: (f32, f32)) {
        self.gate().await;
        let d = lock(&self.w).draw(range);
        tokio::time::sleep(d).await;
        self.gate().await;
    }
}

#[async_trait]
impl BindHost for SimHost {
    fn file_present(&self, path: &str) -> bool {
        let g = lock(&self.w);
        // The upstream artifacts and helper scripts are installed on both rigs.
        path.starts_with("/usr/bin/")
            || path.starts_with("/etc/bind.")
            || g.rigs[self.rig].files.contains_key(path)
    }

    fn on_path(&self, _bin: &str) -> bool {
        true
    }

    async fn generate_keypair(&self) -> Result<(), String> {
        self.take((0.1, 0.8)).await;
        let mut g = lock(&self.w);
        g.keygens += 1;
        let generation = g.keygens;
        let mtime = g.wall_now();
        let files = &mut g.rigs[self.rig].files;
        files.insert(
            UPSTREAM_GS_KEY.to_string(),
            File {
                data: key_blob(generation, 1),
                mtime,
            },
        );
        files.insert(
            UPSTREAM_DRONE_KEY.to_string(),
            File {
                data: key_blob(generation, 0),
                mtime,
            },
        );
        g.log(format!("gs keygen generation {generation}"));
        Ok(())
    }

    async fn reconcile_injection_driver(&self) {
        self.take((0.0, 0.3)).await;
    }

    async fn prepare_injection_iface(&self) -> Vec<InjectionPrepOutcome> {
        self.take((0.2, 2.5)).await;
        vec![InjectionPrepOutcome {
            iface: "wlan1".to_string(),
            injection_mode: "monitor".to_string(),
            monitor_verified: true,
            nm_enumerable: true,
        }]
    }

    async fn reconcile_reg_domain(&self, _events: &EventEmitter) {
        self.gate().await;
    }

    async fn wait_for_iface(&self, _iface: &str, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            self.gate().await;
            let up = lock(&self.w).rigs[self.rig]
                .iface_up_at
                .is_some_and(|t| tokio::time::Instant::now() >= t);
            if up {
                return true;
            }
            tokio::time::sleep(TUNNEL_POLL_INTERVAL).await;
        }
        false
    }

    fn rx_packets(&self, _iface: &str) -> Option<u64> {
        let g = lock(&self.w);
        let r = &g.rigs[self.rig];
        r.iface_up_at
            .is_some_and(|t| tokio::time::Instant::now() >= t)
            .then_some(r.rx_packets)
    }

    async fn kill_stale_socats(&self) {
        self.take((0.01, 0.05)).await;
    }

    async fn run_socat(&self, _args: &[String]) -> Result<(i32, Vec<u8>), SocatFailure> {
        self.gate().await;
        let out = crate::wire::run_socat(&self.w, self.rig).await;
        self.gate().await;
        Ok(out)
    }

    fn wall_now(&self) -> SystemTime {
        lock(&self.w).wall_now()
    }

    fn file_fresh(&self, path: &str, since: SystemTime) -> bool {
        lock(&self.w).rigs[self.rig]
            .files
            .get(path)
            .is_some_and(|f| f.mtime >= since)
    }

    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        lock(&self.w).rigs[self.rig]
            .files
            .get(path)
            .map(|f| f.data.clone())
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
    }

    fn write_key(&self, path: &str, blob: &[u8]) -> Result<(), String> {
        if !self.alive() {
            return Err("process killed".to_string());
        }
        let mut g = lock(&self.w);
        let mtime = g.wall_now();
        g.rigs[self.rig].files.insert(
            path.to_string(),
            File {
                data: blob.to_vec(),
                mtime,
            },
        );
        let name = g.rigs[self.rig].name;
        g.log(format!("{name} writes {path}"));
        Ok(())
    }

    fn read_public_fingerprint(&self, path: &str) -> Result<String, String> {
        match lock(&self.w).rigs[self.rig].files.get(path) {
            Some(f) if f.data.len() == 64 => Ok(hex::encode(&f.data[32..40])),
            Some(_) => Err("wrong size".to_string()),
            None => Err("missing".to_string()),
        }
    }

    fn persist_pair_state(
        &self,
        role: BindRole,
        peer_device_id: Option<&str>,
        paired_at: Option<&str>,
        auto_pair_enabled: Option<bool>,
        fleet: Option<FleetIdentity>,
    ) -> bool {
        if !self.alive() {
            return false;
        }
        let mut g = lock(&self.w);
        let rig = &mut g.rigs[self.rig];
        apply_pair_fields(
            &mut rig.config,
            role,
            peer_device_id,
            paired_at,
            auto_pair_enabled,
            fleet,
        );
        let name = rig.name;
        g.log(format!("{name} persists the pair state"));
        true
    }

    fn persist_fleet(&self, fleet: FleetIdentity) -> bool {
        if !self.alive() {
            return false;
        }
        let mut g = lock(&self.w);
        let rig = &mut g.rigs[self.rig];
        apply_fleet_fields(&mut rig.config, fleet);
        let name = rig.name;
        g.log(format!(
            "{name} keeps the issued slot {} of fleet {}",
            fleet.fleet_slot, fleet.fleet_id
        ));
        true
    }

    fn write_setup_complete(&self, _body: &[u8]) -> Result<(), String> {
        if self.alive() {
            lock(&self.w).rigs[self.rig].setup_complete = true;
        }
        Ok(())
    }

    fn write_sentinel(&self, body: &[u8]) {
        if self.alive() {
            lock(&self.w).rigs[self.rig].set_sentinel(body);
        }
    }

    fn read_sentinel(&self) -> Option<Vec<u8>> {
        lock(&self.w).rigs[self.rig].sentinel.clone()
    }
}

/// One rig's service manager, as seen by one supervisor process generation.
pub struct SimPm(pub SimHost);

impl SimPm {
    /// A `systemctl start|stop`: the job is handed to the service manager and
    /// takes effect when it completes, even if the caller dies meanwhile. A
    /// later job on the same unit replaces one still pending (systemd's
    /// default `--job-mode=replace`), so the last job issued decides.
    async fn act(&self, unit: &str, running: bool) -> bool {
        self.0.gate().await;
        let (w, rig, unit) = (self.0.w.clone(), self.0.rig, unit.to_string());
        let (d, seq) = {
            let mut g = lock(&w);
            let r = g.faults.systemctl_s;
            let d = g.draw(r);
            let seq = g.rigs[rig].jobs.entry(unit.clone()).or_insert(0);
            *seq += 1;
            (d, *seq)
        };
        let job = tokio::spawn(async move {
            tokio::time::sleep(d).await;
            if lock(&w).rigs[rig].jobs.get(&unit) != Some(&seq) {
                return false;
            }
            set(&w, rig, &unit, running)
        });
        let ok = job.await.unwrap_or(false);
        self.0.gate().await;
        ok
    }
}

/// Set `unit` running or stopped, with the tunnel that follows a bind unit.
fn set(w: &Shared, me: usize, unit: &str, running: bool) -> bool {
    let mut g = lock(w);
    let bind_unit = g.rigs[me].role.bind_unit();
    let name = g.rigs[me].name;
    if !g.rigs[me].units.contains_key(unit) {
        return false;
    }
    if unit == bind_unit && running {
        if g.rng.unit() < g.faults.bind_unit_fail {
            g.log(format!("{name} start {unit} fails"));
            return false;
        }
        let never = g.rng.unit() < g.faults.iface_never;
        let r = g.faults.iface_s;
        let up = tokio::time::Instant::now() + g.draw(r);
        let rig = &mut g.rigs[me];
        rig.rx_packets = 0;
        rig.iface_up_at = (!never).then_some(up);
    } else if unit == bind_unit {
        g.rigs[me].iface_up_at = None;
        // The tunnel is gone: this end's unsent segments with it.
        if let Some(c) = g.conn.as_mut() {
            c.queues[1 - me].clear();
            c.open[me] = false;
        }
    }
    g.rigs[me].units.insert(unit.to_string(), running);
    g.log(format!(
        "{name} {} {unit}",
        if running { "starts" } else { "stops" }
    ));
    true
}

#[async_trait]
impl ProcessManager for SimPm {
    async fn start(&self, unit: &str) -> bool {
        self.act(unit, true).await
    }

    async fn stop(&self, unit: &str) -> bool {
        self.act(unit, false).await
    }

    async fn restart(&self, unit: &str) -> bool {
        self.act(unit, true).await
    }

    async fn try_restart(&self, unit: &str) -> bool {
        let running = lock(&self.0.w).rigs[self.0.rig].unit_running(unit);
        if running {
            self.act(unit, true).await
        } else {
            self.0.gate().await;
            false
        }
    }

    async fn reset_failed(&self, _unit: &str) {}

    async fn is_active(&self, unit: &str) -> Option<bool> {
        self.0.take((0.01, 0.1)).await;
        Some(lock(&self.0.w).rigs[self.0.rig].unit_running(unit))
    }

    async fn mask(&self, _unit: &str) {}

    async fn unmask(&self, _unit: &str) {}
}

/// The handles a freshly started supervisor process of `rig` gets.
pub fn boot_handles(w: &Shared, rig: usize) -> (Arc<SimHost>, Arc<SimPm>) {
    let epoch = lock(w).rigs[rig].epoch;
    let host = SimHost {
        w: w.clone(),
        rig,
        epoch,
    };
    (Arc::new(host.clone()), Arc::new(SimPm(host)))
}
