//! Granting a freshly installed extension its required permissions and
//! enabling it.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use ados_plugin_host::sandbox::NETWORK_OUTBOUND_CAP;
use ados_plugin_host::PluginSupervisor;

use super::ATTEMPTS;

/// Grant every required permission `plugin_id` does not hold yet, then enable
/// it when `enable` is set (an install that must stay off records its grants
/// only). A grant is retried up to [`ATTEMPTS`] times; a `network.outbound`
/// grant waits for the loopback guard before each try. Returns the permissions
/// this call granted.
pub(super) fn grant_required_and_enable(
    supervisor: &mut PluginSupervisor,
    plugin_id: &str,
    guard_wait: Duration,
    enable: bool,
) -> Result<Vec<String>, String> {
    let required = required_permissions(supervisor, plugin_id)?;
    let held: BTreeSet<String> = supervisor
        .find_install(plugin_id)
        .map(|i| {
            i.permissions
                .iter()
                .filter(|(_, g)| g.granted)
                .map(|(id, _)| id.clone())
                .collect()
        })
        .unwrap_or_default();
    let mut granted = Vec::new();
    for permission in required.difference(&held) {
        let mut last = String::new();
        let mut ok = false;
        for attempt in 1..=ATTEMPTS {
            if permission == NETWORK_OUTBOUND_CAP {
                wait_for_loopback_guard(&supervisor.paths().loopback_guard_state, guard_wait);
            }
            match supervisor.grant_permission(plugin_id, permission) {
                Ok(_) => {
                    ok = true;
                    break;
                }
                Err(e) => {
                    tracing::warn!(attempt, %permission, error = %e, "extension grant failed");
                    last = e.to_string();
                }
            }
        }
        if !ok {
            return Err(not_enabled(
                plugin_id,
                &format!("granting {permission} failed: {last}"),
            ));
        }
        granted.push(permission.clone());
    }
    if enable {
        supervisor
            .enable(plugin_id)
            .map_err(|e| not_enabled(plugin_id, &format!("enable failed: {e}")))?;
    }
    Ok(granted)
}

/// Every permission either half of the installed manifest marks `required`.
fn required_permissions(
    supervisor: &PluginSupervisor,
    plugin_id: &str,
) -> Result<BTreeSet<String>, String> {
    let manifest = supervisor
        .installed_manifest(plugin_id)
        .map_err(|e| format!("reading the installed manifest failed: {e}"))?;
    let agent = manifest.agent.iter().flat_map(|a| a.permissions.iter());
    let gcs = manifest.gcs.iter().flat_map(|g| g.permissions.iter());
    Ok(agent
        .chain(gcs)
        .filter(|p| p.required)
        .map(|p| p.id.clone())
        .collect())
}

/// The message for a plugin that installed but could not be switched on. A
/// re-run of the installer finishes it; the manual path is named too.
fn not_enabled(plugin_id: &str, why: &str) -> String {
    format!(
        "{plugin_id} is installed but not enabled ({why}); re-run the installer to finish it, \
         or enable it from Mission Control's plugin page or with `ados plugin enable {plugin_id}`"
    )
}

/// Wait up to `timeout` for the plugin host to report its loopback guard
/// active. Returns either way: a guard that never comes up is reported by the
/// grant it blocks, with the guard's own reason.
fn wait_for_loopback_guard(state: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !ados_protocol::plugin_loopback_guard::read_state_at(state).active
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(500));
    }
}
