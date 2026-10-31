//! Flight-controller device watch and tamper marking.

use super::*;

impl<H: HostServices> PluginReconciler<H> {
    /// Record the router's live FC device and re-render the plugin units when
    /// it moved. A missing socket or a null device (a network source, or a
    /// link down between reconnects) keeps the last recorded device masked.
    pub async fn watch_fc_device(&self) {
        let Some((units, socket, run_dir)) = &self.units else {
            return;
        };
        let Some((_, snapshot)) =
            ados_protocol::armed::read_state_snapshot(socket, FC_STATE_READ_TIMEOUT).await
        else {
            return;
        };
        let Some(device) = snapshot.get("fc_device").and_then(|v| v.as_str()) else {
            return;
        };
        if !device.starts_with("/dev/") {
            return;
        }
        match crate::systemd::record_live_fc_device(run_dir, device) {
            Ok(false) => {}
            Ok(true) => {
                tracing::info!(
                    fc_device = device,
                    "fc device moved; re-rendering plugin units"
                );
                let units = units.clone();
                if let Err(e) = tokio::task::spawn_blocking(move || units.refresh_units()).await {
                    tracing::warn!(error = %e, "plugin unit refresh task failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not record the live fc device"),
        }
    }

    /// Record each tampered plugin as `Failed` with the tamper reason and stop
    /// its units. The socket and token go in the same pass, because a failed
    /// plugin is no longer wanted.
    pub(super) fn mark_tampered(&self, tampered: &[(String, String)]) {
        for (id, detail) in tampered {
            tracing::error!(plugin_id = %id, detail = %detail, "plugin_tampered; stopped serving it");
        }
        match state::StateLock::acquire(Some(&self.state_path)) {
            Ok(_lock) => match state::load_state_checked(Some(&self.state_path)) {
                Ok(mut installs) => {
                    for (id, detail) in tampered {
                        if let Some(install) = state::find_install_mut(&mut installs, id) {
                            install.status = PluginStatus::Failed;
                            install.failure_reason = Some(format!("{TAMPERED_REASON}: {detail}"));
                        }
                    }
                    if let Err(e) = state::save_state(&installs, Some(&self.state_path)) {
                        tracing::warn!(error = %e, "could not record the tampered plugins");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "plugin state unreadable; tamper not recorded")
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "plugin state lock unavailable; tamper not recorded")
            }
        }
        if let Some((units, _, _)) = &self.units {
            for (id, _) in tampered {
                units.stop_plugin_units(id);
            }
        }
    }
}
