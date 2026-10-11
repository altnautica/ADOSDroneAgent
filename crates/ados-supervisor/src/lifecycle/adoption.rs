//! Periodic adoption of units systemd already runs.

use super::*;

impl Supervisor {
    /// Promote gate-allowed catalog rows that are already `active` into the
    /// supervised set, so the monitor's death detection covers them.
    ///
    /// Only ever promotes: a row that is not active stays `Stopped` and is
    /// never started here. That is what makes the sweep safe — a unit the
    /// operator never opted into (an unset marker, an unmet
    /// `ConditionPathExists`) is inactive, so it is not adopted and the
    /// supervisor does not start it.
    pub(super) async fn adopt_active_units(&mut self) {
        let candidates: Vec<&'static str> = self
            .services
            .iter()
            .filter(|spec| {
                matches!(spec.state, ServiceState::Stopped | ServiceState::Idle)
                    && !SELF_TERMINATING_UNITS.contains(&spec.name)
                    && gate_allows(spec, &self.config)
            })
            .map(|spec| spec.name)
            .collect();
        for name in candidates {
            // Only a confirmed-active unit is adopted; an unreadable one is
            // left for the next sweep.
            let active = self.pm.is_active(name).await == Some(true);
            self.progress.mark();
            if !active {
                continue;
            }
            if let Some(i) = self.index_of(name) {
                self.services[i].adopted = true;
                self.set_state(i, ServiceState::Running, "adopted_already_active");
                tracing::info!(service = name, "adopted a unit this process did not start");
            }
        }
    }
}
