//! Follows the drone's control mode from the ground-station input sidecar
//! (`gs-input-state.json`, written by `ados-input`) into the source merge.
//!
//! The sidecar is a tmpfs file, so a 100 ms read keeps the ground-station
//! emergency switch (L2 + R2 + Share, which latches `elrs` in the sidecar at
//! once) effectively immediate on this lane.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{watch, Mutex};

use crate::sources::{ControlMode, SourceMerge};

/// The read cadence.
pub const POLL: Duration = Duration::from_millis(100);

/// The sidecar path under the resolved runtime dir.
pub fn sidecar_path() -> PathBuf {
    PathBuf::from(crate::paths::run_path(
        ados_hid::pilot_input::status::GS_INPUT_STATE,
    ))
}

/// Apply the sidecar's mode to the merge, logging a change. Returns the mode.
pub async fn apply(path: &Path, merge: &Mutex<SourceMerge>) -> ControlMode {
    let mode = ados_hid::pilot_input::status::read_control_mode(path);
    let mut m = merge.lock().await;
    if m.control_mode() != mode {
        tracing::info!(
            from = m.control_mode().as_str(),
            to = mode.as_str(),
            "crsf_control_mode"
        );
        m.set_control_mode(mode);
    }
    mode
}

/// Run until the pass scope closes.
pub async fn run(path: PathBuf, merge: Arc<Mutex<SourceMerge>>, mut scope: watch::Receiver<bool>) {
    loop {
        apply(&path, &merge).await;
        tokio::select! {
            biased;
            r = scope.wait_for(|s| *s) => { let _ = r; return; }
            _ = tokio::time::sleep(POLL) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::{ChannelSource, ChannelSourceMode};
    use ados_hid::pilot_input::status::{GsInputState, ModeSource};

    #[tokio::test]
    async fn the_sidecar_mode_selects_the_lane_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gs-input-state.json");
        let merge = Mutex::new(SourceMerge::new(ChannelSourceMode::Hid));
        // No sidecar: the schema default `ados`, so the lane sends neutral.
        assert_eq!(apply(&path, &merge).await, ControlMode::Ados);
        let now = std::time::Instant::now();
        assert_eq!(
            merge.lock().await.current(now).unwrap().1,
            ChannelSource::Neutral
        );

        let mut s = GsInputState {
            v: 1,
            ts_ms: 0,
            native_sticks: true,
            pilot_relay: true,
            streaming: false,
            control_mode: ControlMode::Elrs,
            control_mode_source: ModeSource::Emergency,
            controller: None,
        };
        s.save(&path).unwrap();
        assert_eq!(apply(&path, &merge).await, ControlMode::Elrs);
        assert_eq!(
            merge.lock().await.current(now),
            None,
            "elrs with no live pad: silence"
        );

        s.control_mode = ControlMode::FcManual;
        s.save(&path).unwrap();
        assert_eq!(apply(&path, &merge).await, ControlMode::FcManual);
        assert_eq!(
            merge.lock().await.current(now).unwrap().1,
            ChannelSource::Neutral
        );
    }
}
