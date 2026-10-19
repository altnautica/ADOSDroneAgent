//! The capture category's state across janitor passes: the last sidecar, the
//! resolved root, and the write probe running on a blocking thread.

use std::path::{Path, PathBuf};
use std::time::Instant;

use ados_config::capture_storage::{
    CaptureStorageConfig, CaptureStorageSidecar, SysRoots, WriteProbe,
};
use tokio::task::JoinHandle;

use super::{measure_write, mono_ns, probe_due, run_pass, write_sidecar, PROBE_BYTES};

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct CaptureRuntime {
    started: Instant,
    /// Whether this node's profile records capture sessions, as of the last pass.
    active: bool,
    root: Option<PathBuf>,
    last: Option<CaptureStorageSidecar>,
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
            last: None,
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

    /// One capture pass on a blocking thread (directory walks and session
    /// removal), then the sidecar write. Returns the bytes evicted.
    pub async fn pass(
        &mut self,
        cfg: CaptureStorageConfig,
        evict: bool,
        sidecar_path: &Path,
    ) -> u64 {
        let probe = self.probe.clone();
        let joined = tokio::task::spawn_blocking(move || {
            run_pass(&cfg, &SysRoots::default(), evict, probe, mono_ns())
        })
        .await;
        let pass = match joined {
            Ok(pass) => pass,
            Err(e) => {
                tracing::warn!(error = %e, "capture storage pass did not complete");
                return 0;
            }
        };
        if let Err(e) = write_sidecar(sidecar_path, &pass.sidecar) {
            tracing::debug!(error = %e, "capture-storage sidecar write failed");
        }
        self.root = Some(pass.root);
        self.last = Some(pass.sidecar);
        pass.freed
    }

    /// Collect a finished probe into the sidecar, or start one when due. Cheap
    /// when neither applies; never waits on the probe itself.
    pub async fn service_probe(&mut self, sidecar_path: &Path) {
        if let Some(task) = self.probe_task.take() {
            if !task.is_finished() {
                self.probe_task = Some(task);
                return;
            }
            self.probe = task.await.ok().flatten();
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
        self.last_probe = Some(now);
        // The probe must not be what pushes the volume under its floor.
        let room = last.volume.as_ref().is_some_and(|v| {
            v.free_bytes >= last.capture.floor_free_bytes.saturating_add(PROBE_BYTES)
        });
        if !room {
            return;
        }
        self.probe_task = Some(tokio::task::spawn_blocking(move || {
            measure_write(&root, PROBE_BYTES).map(|mbps| WriteProbe {
                mbps,
                measured_at_ns: mono_ns(),
            })
        }));
    }
}
