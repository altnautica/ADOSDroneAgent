//! Plugin-declared extra services (`agent.contributes.services`).
//!
//! A subprocess agent half may declare long-running helper processes beside
//! its main runner. Each one gets its own systemd unit, rendered here with the
//! same slice, hardening, resource envelope and capability sandbox as the main
//! unit, and an optional readiness check the supervisor probes after enable.
//!
//! The manifest shape and every rule below match the Python model
//! (`ados.plugins.manifest.ServiceSpec`, `ados.plugins.systemd`,
//! `ados.plugins.ready_check`), so a plugin behaves the same whether the LAN
//! path or the cloud path enabled it.

mod argv;
mod declare;
mod probe;

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::time::Duration;

use serde::Deserialize;

use crate::backend::{ProbeSpec, UnitSpec};
use crate::errors::SupervisorError;
use crate::manifest::{bin_reference, canonical_profile, AgentIsolation, PluginManifest};
use crate::sandbox::{
    capture_root_bind, fc_port_exclusion, sandbox_directives, NETWORK_LISTEN_CAP,
    NETWORK_OUTBOUND_CAP,
};
use crate::supervisor::Paths;
use crate::systemd::{
    capture_root, exec_paths_for, fc_serial_port, path_token, plugin_unit_basename,
    resolve_program, runner_context, sanitize_unit_name, service_log_path_for, PLUGIN_SLICE_NAME,
    PLUGIN_UNIT_PREFIX,
};

use argv::split_argv;

pub use declare::{declared_services, parse_ready_check};
pub use probe::{probe_http, probe_spec};

/// How long an HTTP readiness probe may take.
const HTTP_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The only host an HTTP readiness probe may name.
const LOOPBACK_HOST: &str = "127.0.0.1";

/// Most TCP ports one service may declare.
pub const MAX_LISTEN_PORTS: usize = 4;

/// One declared service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSpec {
    pub name: String,
    pub command: String,
    pub ready_check: Option<ReadyCheck>,
    pub restart: String,
    /// Node profiles the service runs on; `None` means every profile.
    pub profiles: Option<Vec<String>>,
    /// TCP ports the service serves, opened by the `network.listen` grant.
    pub listen_ports: Vec<u16>,
}

impl ServiceSpec {
    /// Whether the service runs on a node of `profile`.
    pub fn applies_to(&self, profile: &str) -> bool {
        self.profiles
            .as_ref()
            .is_none_or(|p| p.iter().any(|x| x == profile))
    }
}

/// A parsed `ready_check`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadyCheck {
    /// GET `http://127.0.0.1:<port><path>`; ready on a 2xx.
    Http {
        url: String,
        port: u16,
        path: String,
    },
    /// Run the argv sandboxed; ready on exit 0.
    Command(Vec<String>),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawService {
    Bare(String),
    Full {
        name: String,
        command: String,
        #[serde(default)]
        ready_check: Option<String>,
        #[serde(default = "default_restart")]
        restart: String,
        #[serde(default = "default_slice")]
        slice: String,
        #[serde(default)]
        profiles: Option<Vec<String>>,
        #[serde(default)]
        listen_ports: Vec<u16>,
    },
}

fn default_restart() -> String {
    "on-failure".to_string()
}

fn default_slice() -> String {
    PLUGIN_SLICE_NAME.to_string()
}

/// Split a plugin-authored `command` into its argv, refusing a control
/// character (a newline would start a new unit directive), an empty command, a
/// first word carrying a systemd exec prefix, and a lone `;` word (systemd's
/// command separator). Each word is later re-quoted by [`exec_word`] with `%`
/// and `$` doubled, so no specifier or variable expands.
pub fn service_argv(command: &str) -> Result<Vec<String>, SupervisorError> {
    if command.chars().any(char::is_control) {
        return Err(SupervisorError(
            "service command must not contain control characters".to_string(),
        ));
    }
    let argv = split_argv(command)
        .map_err(|e| SupervisorError(format!("service command is not a valid argv: {e}")))?;
    let Some(first) = argv.first().filter(|w| !w.is_empty()) else {
        return Err(SupervisorError(
            "service command must not be empty".to_string(),
        ));
    };
    if first.starts_with(['-', '@', ':', '+', '!', '|']) {
        return Err(SupervisorError(format!(
            "service command must not start with a systemd prefix ({:?})",
            &first[..1]
        )));
    }
    if argv.iter().any(|w| w == ";") {
        return Err(SupervisorError(
            "service command must not contain a lone ';' word".to_string(),
        ));
    }
    Ok(argv)
}

/// Quote one exec word for a systemd `ExecStart=` line: a plain word passes
/// through, anything else is double-quoted with `%` and `$` doubled.
pub(crate) fn exec_word(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./:=,@+-".contains(c));
    if plain {
        return word.to_string();
    }
    let escaped = word
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$");
    format!("\"{escaped}\"")
}

/// The unit name for a declared service, distinct from the main unit by its
/// trailing `-<service>` segment after the plugin's hashed basename.
pub fn service_unit_name_for(plugin_id: &str, service_name: &str) -> String {
    format!(
        "{PLUGIN_UNIT_PREFIX}{}-{}.service",
        plugin_unit_basename(plugin_id),
        sanitize_unit_name(service_name)
    )
}

/// Build the unit for one declared service: its own exec line in the plugin's
/// install dir (a leading `bin:<name>` resolved to this host's binary), the
/// plugin's identity (host socket, token file, binds), the shared plugin
/// slice, the main unit's hardening and resource envelope, and the plugin's
/// capability sandbox with the service's declared listen ports.
pub fn build_service_spec(
    manifest: &PluginManifest,
    service: &ServiceSpec,
    paths: &Paths,
    profile: &str,
    granted: &BTreeSet<String>,
    loopback_guard_active: bool,
) -> Result<UnitSpec, SupervisorError> {
    let Some(agent) = manifest.agent.as_ref() else {
        return Err(SupervisorError(format!(
            "plugin {} has no agent half; no service unit needed",
            manifest.id
        )));
    };
    let working_dir = paths.install_dir.join(&manifest.id);
    if working_dir
        .to_string_lossy()
        .chars()
        .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(SupervisorError(format!(
            "refusing to render a service unit: install dir {} is not a single token",
            working_dir.display()
        )));
    }
    let mut argv = service_argv(&service.command)?;
    if bin_reference(&argv[0]).is_some() {
        argv[0] = resolve_program(manifest, &argv[0], paths)?;
    }
    let mut spec = runner_context(manifest, paths, profile)?;
    spec.description = format!("ADOS plugin {} service {}", manifest.id, service.name);
    spec.argv = argv;
    spec.working_dir = Some(working_dir);
    spec.log_path = service_log_path_for(&paths.log_dir, &manifest.id, &service.name);
    spec.restart = service.restart.clone();
    spec.resources = agent.resources.clone();
    spec.sandbox_directives =
        sandbox_directives(granted, loopback_guard_active, &service.listen_ports);
    spec.sandbox_directives
        .extend(fc_port_exclusion(granted, fc_serial_port().as_deref()));
    spec.sandbox_directives
        .extend(capture_root_bind(granted, capture_root().as_deref()));
    Ok(spec)
}

#[cfg(test)]
mod tests;
