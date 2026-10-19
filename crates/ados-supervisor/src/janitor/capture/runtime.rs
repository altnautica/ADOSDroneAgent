//! The capture category's state across janitor ticks: the last sidecar, the
//! resolved root, the settled-session size cache, the refresh cadence, and
//! the write probe running on a blocking thread.

use std::path::{Path, PathBuf};
use std::time::Instant;

use ados_config::capture_storage::{
    CaptureStorageConfig, CaptureStorageSidecar, SysRoots, WriteProbe, SIDECAR_REFRESH,
};
use tokio::task::JoinHandle;

use super::{
    measure_write, mono_ns, probe_due, run_pass, write_sidecar, PassMode, PassTime, SizeCache,
    PROBE_BYTES,
};
use crate::sdnotify::MonitorProgress;

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct CaptureRuntime {
    started: Instant,
    /// Whether this node's profile records capture sessions, as of the last pass.
    active: bool,
    root: Option<PathBuf>,
    /// The last capture-root creation failure, logged once per distinct error.
    root_error: Option<String>,
    last: Option<CaptureStorageSidecar>,
    last_refresh: Option<Instant>,
    cache: SizeCache,
    probe: Option<WriteProbe>,
    last_probe: Option<Instant>,
    probe_task: Option<JoinHandle<Option<WriteProbe>>>,
}

impl Default for CaptureRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl CaptureRuntime {
    pub fn new() -> Self {
        CaptureRuntime {
            started: Instant::now(),
            active: false,
            root: None,
            root_error: None,
            last: None,
            last_refresh: None,
            cache: SizeCache::new(),
            probe: None,
            last_probe: None,
            probe_task: None,
        }
    }

    pub fn active(&self) -> bool {
        self.active
    }

    pub fn set_active(&mut self, active: bool) {
        self.active = active;
    }

    /// Whether the sidecar is due its short-cadence rewrite at `now`.
    pub fn refresh_due(&self, now: Instant) -> bool {
        self.last_refresh
            .is_none_or(|t| now.duration_since(t) >= SIDECAR_REFRESH)
    }

    /// One capture pass on a blocking thread (directory walks and session
    /// removal), then the sidecar write. `progress` is stamped between
    /// sessions so a long pass keeps the watchdog fed. Returns the bytes
    /// evicted.
    pub async fn pass(
        &mut self,
        cfg: CaptureStorageConfig,
        mode: PassMode,
        sidecar_path: &Path,
        progress: MonitorProgress,
    ) -> u64 {
        self.last_refresh = Some(Instant::now());
        let probe = self.probe.clone();
        let mut cache = std::mem::take(&mut self.cache);
        let joined = tokio::task::spawn_blocking(move || {
            let keepalive = move || progress.mark();
            let pass = run_pass(
                &cfg,
                &SysRoots::default(),
                mode,
                probe,
                &mut cache,
                PassTime::now(),
                &keepalive,
            );
            (pass, cache)
        })
        .await;
        let pass = match joined {
            Ok((pass, cache)) => {
                self.cache = cache;
                pass
            }
            Err(e) => {
                tracing::warn!(error = %e, "capture storage pass did not complete");
                return 0;
            }
        };
        self.note_root_error(&pass.root, pass.root_error.as_deref());
        if let Err(e) = write_sidecar(sidecar_path, &pass.sidecar) {
            tracing::debug!(error = %e, "capture-storage sidecar write failed");
        }
        self.root = Some(pass.root);
        self.last = Some(pass.sidecar);
        pass.freed
    }

    /// Log a capture root that cannot be created once per distinct error, and
    /// its recovery, rather than every 10 s refresh.
    fn note_root_error(&mut self, root: &Path, error: Option<&str>) {
        if self.root_error.as_deref() == error {
            return;
        }
        match error {
            Some(e) => tracing::warn!(
                root = %root.display(),
                error = e,
                "capture root cannot be created; new capture sessions are refused"
            ),
            None => tracing::info!(root = %root.display(), "capture root is writable again"),
        }
        self.root_error = error.map(str::to_string);
    }

    /// Collect a finished probe into the sidecar, or start one when due. Cheap
    /// when neither applies; never waits on the probe itself. A failed probe
    /// keeps the last good measurement.
    pub async fn service_probe(&mut self, sidecar_path: &Path) {
        if let Some(task) = self.probe_task.take() {
            if !task.is_finished() {
                self.probe_task = Some(task);
                return;
            }
            let Some(measured) = task.await.ok().flatten() else {
                return;
            };
            self.probe = Some(measured);
            if let Some(sidecar) = self.last.as_mut() {
                sidecar.write_probe = self.probe.clone();
                sidecar.updated_ns = mono_ns();
                if let Err(e) = write_sidecar(sidecar_path, sidecar) {
                    tracing::debug!(error = %e, "capture-storage sidecar write failed");
                }
            }
            return;
        }
        let (Some(root), Some(last)) = (self.root.clone(), self.last.as_ref()) else {
            return;
        };
        let now = Instant::now();
        let since_last = self.last_probe.map(|t| now.duration_since(t));
        if !probe_due(now.duration_since(self.started), since_last) {
            return;
        }
        // The probe must not be what pushes the volume under its floor, and a
        // root that cannot be created has nothing to measure. Neither counts
        // as a probe: the next tick checks again.
        let room = last.volume.as_ref().is_some_and(|v| {
            v.free_bytes >= last.capture.floor_free_bytes.saturating_add(PROBE_BYTES)
        });
        if !room || self.root_error.is_some() {
            return;
        }
        self.last_probe = Some(now);
        self.probe_task = Some(tokio::task::spawn_blocking(move || {
            measure_write(&root, PROBE_BYTES).map(|mbps| WriteProbe {
                mbps,
                measured_at_ns: mono_ns(),
            })
        }));
    }
}

#[cfg(test)]
mod tests;
