//! Permission grants and unit refresh after a permission change.

use super::*;

impl PluginSupervisor {
    /// Grant a declared permission. Rejects a permission the manifest does not
    /// declare, and a permission whose capability the active host runtime cannot
    /// back (see [`with_ungrantable_caps`](Self::with_ungrantable_caps)) so an
    /// operator never grants a capability that can only error at call time.
    pub fn grant_permission(
        &mut self,
        plugin_id: &str,
        permission_id: &str,
    ) -> Result<(), LifecycleError> {
        let lock = StateLock::acquire(Some(&self.paths.state_path))?;
        self.reload_installs()?;
        let manifest = self.manifest_for(plugin_id)?;
        if !manifest.declared_permissions().contains(permission_id) {
            return Err(SupervisorError(format!(
                "plugin {plugin_id} did not declare permission {permission_id}"
            ))
            .into());
        }
        // Refuse a capability the active host runtime cannot back: its host
        // method returns not_implemented regardless of wiring, so granting it
        // would only surface a surprise error when the plugin first calls the
        // gated method. Refuse honestly at grant time instead.
        if self.ungrantable_caps.contains(permission_id) {
            return Err(SupervisorError(format!(
                "plugin {plugin_id}: capability {permission_id} is not supported by this \
                 agent runtime (its host method is not implemented); refusing to grant a \
                 capability that can only error at call time"
            ))
            .into());
        }
        // Network access is only safe to hold while the loopback guard keeps
        // the plugin off the agent's own listeners. A backend that enforces no
        // sandbox runs first-party plugins only, with no socket policy for the
        // guard to complement.
        if permission_id == crate::sandbox::NETWORK_OUTBOUND_CAP && self.backend.enforces_sandbox()
        {
            let guard = ados_protocol::plugin_loopback_guard::read_state_at(
                &self.paths.loopback_guard_state,
            );
            if !guard.active {
                return Err(SupervisorError(format!(
                    "plugin {plugin_id}: {permission_id} refused: the plugin loopback guard is \
                     unavailable ({}), so a network-capable plugin could reach the agent's own \
                     loopback services",
                    guard.reason
                ))
                .into());
            }
        }
        let install = self.require_install_mut(plugin_id)?;
        grant_permission(install, permission_id);
        save_state(&self.installs, Some(&self.paths.state_path))?;
        // Release the state lock before re-entering the state path.
        drop(lock);
        self.apply_permission_change(plugin_id, &manifest)?;
        Ok(())
    }

    /// Revoke a granted permission.
    ///
    /// Takes effect immediately: the sandbox half of the unit is re-rendered
    /// and the plugin's capability token is re-minted from the new grant set
    /// (see [`apply_permission_change`](Self::apply_permission_change)). It
    /// used to take effect only on the next plugin-host restart, so an
    /// operator revoking `mavlink.write` from a misbehaving plugin saw success
    /// in the CLI and the GCS while the plugin kept commanding the flight
    /// controller.
    pub fn revoke_permission(
        &mut self,
        plugin_id: &str,
        permission_id: &str,
    ) -> Result<(), LifecycleError> {
        let lock = StateLock::acquire(Some(&self.paths.state_path))?;
        self.reload_installs()?;
        let manifest = self.manifest_for(plugin_id)?;
        let install = self.require_install_mut(plugin_id)?;
        revoke_permission(install, permission_id);
        save_state(&self.installs, Some(&self.paths.state_path))?;
        // Release the state lock before re-entering the state path.
        drop(lock);
        self.apply_permission_change(plugin_id, &manifest)?;
        Ok(())
    }

    /// Re-render every installed plugin's unit against the current grant set
    /// and loopback-guard verdict, restarting a running plugin whose sandbox
    /// changed. The plugin-host daemon calls it at startup, after loading the
    /// guard, so a unit rendered while the guard was active cannot keep network
    /// access into a boot where it is not. It also re-creates each HTTP
    /// plugin's socket dir, which lives on the run-dir tmpfs.
    pub fn refresh_all_units(&self) {
        let ids: Vec<String> = self.installs.iter().map(|i| i.plugin_id.clone()).collect();
        for plugin_id in ids {
            let result = self
                .manifest_for(&plugin_id)
                .map_err(LifecycleError::from)
                .and_then(|m| self.refresh_unit(&plugin_id, &m));
            if let Err(e) = result {
                tracing::error!(plugin_id, error = %e, "plugin_unit_refresh_failed");
            }
        }
    }

    /// The per-plugin loopback-guard exceptions: each installed plugin granted
    /// `network.listen`, with the TCP ports its services on this profile
    /// declare, keyed by that plugin's system uid. A plugin whose user does
    /// not exist yet gets none.
    pub fn loopback_guard_exceptions(&self) -> Vec<crate::loopback_guard::ListenException> {
        self.installs
            .iter()
            .filter(|i| state::granted_caps(i).contains(crate::sandbox::NETWORK_LISTEN_CAP))
            .filter_map(|install| {
                let manifest = self.manifest_for(&install.plugin_id).ok()?;
                let tcp_ports: Vec<u16> = self
                    .active_services_of(&install.plugin_id, &manifest)
                    .into_iter()
                    .flat_map(|s| s.listen_ports)
                    .collect();
                if tcp_ports.is_empty() {
                    return None;
                }
                let uid = crate::plugin_account::plugin_uid(&install.plugin_id)?;
                Some(crate::loopback_guard::ListenException { uid, tcp_ports })
            })
            .collect()
    }

    /// Rewrite one plugin's units when their rendered sandbox differs from
    /// what is installed, then restart the running ones (the service manager
    /// applies the sandbox at exec).
    pub(super) fn refresh_unit(
        &self,
        plugin_id: &str,
        manifest: &PluginManifest,
    ) -> Result<(), LifecycleError> {
        if !manifest.is_subprocess_agent() {
            return Ok(());
        }
        self.ensure_account(manifest)?;
        self.ensure_http_dir(manifest)?;
        self.ensure_data_dir(manifest)?;
        let granted = self
            .find_install(plugin_id)
            .map(state::granted_caps)
            .unwrap_or_default();
        let guard =
            ados_protocol::plugin_loopback_guard::read_state_at(&self.paths.loopback_guard_state);
        let running = self
            .find_install(plugin_id)
            .is_some_and(|i| matches!(i.status, PluginStatus::Running));
        if let Some(spec) = build_unit_spec(
            manifest,
            &self.paths,
            &self.current_profile,
            &granted,
            guard.active,
        )? {
            let name = unit_name_for(plugin_id);
            if self.backend.install(&name, &spec)? && running {
                self.backend.restart(&name)?;
            }
        }
        // Declared services run under the same capability sandbox as the main
        // unit, so they follow every grant and revoke too. A plugin that is not
        // running has its services rendered afresh when it next starts.
        if !running {
            return Ok(());
        }
        for service in self.active_services_of(plugin_id, manifest) {
            let spec = match build_service_spec(
                manifest,
                &service,
                &self.paths,
                &self.current_profile,
                &granted,
                guard.active,
            ) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(plugin_id, service = %service.name, error = %e, "plugin_service_render_failed");
                    continue;
                }
            };
            let name = service_unit_name_for(plugin_id, &service.name);
            if self.backend.install(&name, &spec)? {
                self.backend.restart(&name)?;
            }
        }
        Ok(())
    }

    /// Make a permission change real, right now.
    ///
    /// Two mechanisms enforce a plugin's capabilities and both are snapshots
    /// taken when something was last written, so both have to be refreshed:
    ///
    /// 1. **The sandbox.** `hardware.*`, `network.*` and `filesystem.host` live
    ///    in the generated unit (see [`crate::sandbox`]), so the unit is
    ///    re-rendered and, when the grant set actually changed the text, the
    ///    plugin is restarted to pick up the new namespace. A restart is the
    ///    only way: the service manager applies `DeviceAllow`/
    ///    `RestrictAddressFamilies` at exec time.
    /// 2. **The capability token.** Every wire-gated capability rides in the
    ///    plugin's HMAC token. The live plugin host re-mints it through the
    ///    control socket, which also pushes the fresh token to the open
    ///    connection so the next request re-gates against the new set.
    ///
    /// A restart is skipped when the unit text is unchanged (a purely
    /// wire-gated capability), because bouncing a running geofence plugin to
    /// apply a token change it can receive live is a needless gap in coverage.
    pub(super) fn apply_permission_change(
        &self,
        plugin_id: &str,
        manifest: &PluginManifest,
    ) -> Result<(), LifecycleError> {
        self.refresh_unit(plugin_id, manifest)?;

        // Re-mint the live token. A plugin host that is not up has nothing to
        // re-mint against and will read the new grant set off state when it
        // starts, so an unreachable control socket is logged, not an error. The
        // host's state poll also compares each live session's grant set with
        // state, so a missed poke still lands within one poll period.
        match crate::rotate_token_via_control(&self.paths.control_dir, plugin_id) {
            Ok(true) => tracing::info!(plugin_id, "plugin_token_rotated_after_permission_change"),
            Ok(false) => tracing::info!(
                plugin_id,
                "plugin token re-minted; no live session took it, the plugin reads it on connect"
            ),
            Err(e) => tracing::warn!(
                plugin_id,
                detail = %e,
                "plugin host control socket unreachable; the host's state poll applies \
                 the new grant set to the live session"
            ),
        }
        Ok(())
    }
}
