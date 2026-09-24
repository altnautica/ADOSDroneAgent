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

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::time::Duration;

use serde::Deserialize;

use crate::backend::{ProbeSpec, UnitSpec};
use crate::errors::SupervisorError;
use crate::manifest::{bin_reference, canonical_profile, AgentIsolation, PluginManifest};
use crate::sandbox::{sandbox_directives, NETWORK_LISTEN_CAP, NETWORK_OUTBOUND_CAP};
use crate::supervisor::Paths;
use crate::systemd::{
    path_token, resolve_program, runner_context, sanitize_unit_name, service_log_path_for,
    PLUGIN_SLICE_NAME, PLUGIN_UNIT_PREFIX,
};

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

/// The services a manifest declares, validated. Only a subprocess agent half
/// gets extra units; anything else declares none. A bare string element is the
/// legacy shape and means `{name: s, command: s}`.
///
/// Beyond the shape, a declared service is refused when its command names a
/// `bin:` entry the manifest's `binaries` does not have, when it names an
/// unknown profile, or when it declares listen ports outside 1024..=65535,
/// more than [`MAX_LISTEN_PORTS`], twice, or without the plugin declaring both
/// `network.listen` (the bind grant) and `network.outbound` (the inet socket
/// families a listener needs).
pub fn declared_services(manifest: &PluginManifest) -> Result<Vec<ServiceSpec>, SupervisorError> {
    let Some(agent) = manifest.agent.as_ref() else {
        return Ok(Vec::new());
    };
    if agent.isolation != AgentIsolation::Subprocess {
        return Ok(Vec::new());
    }
    let Some(raw) = agent
        .extra
        .get("contributes")
        .and_then(|c| c.get("services"))
    else {
        return Ok(Vec::new());
    };
    let raw: Vec<RawService> = serde_norway::from_value(raw.clone()).map_err(|e| {
        SupervisorError(format!(
            "plugin {}: agent.contributes.services is malformed: {e}",
            manifest.id
        ))
    })?;
    let declared = manifest.declared_permissions();
    raw.into_iter()
        .map(|r| {
            let (name, command, ready_check, restart, slice, profiles, listen_ports) = match r {
                RawService::Bare(s) => (
                    s.clone(),
                    s,
                    None,
                    default_restart(),
                    default_slice(),
                    None,
                    Vec::new(),
                ),
                RawService::Full {
                    name,
                    command,
                    ready_check,
                    restart,
                    slice,
                    profiles,
                    listen_ports,
                } => (
                    name,
                    command,
                    ready_check,
                    restart,
                    slice,
                    profiles,
                    listen_ports,
                ),
            };
            validate_name(&name)?;
            if slice != PLUGIN_SLICE_NAME {
                return Err(SupervisorError(format!(
                    "service slice {slice:?} is not allowed; plugin services run in \
                     {PLUGIN_SLICE_NAME}"
                )));
            }
            if !matches!(restart.as_str(), "always" | "on-failure" | "no") {
                return Err(SupervisorError(format!(
                    "service {name}: restart {restart:?} must be always, on-failure or no"
                )));
            }
            if command.is_empty() {
                return Err(SupervisorError(format!("service {name}: empty command")));
            }
            let argv = service_argv(&command)?;
            if let Some(bin) = bin_reference(&argv[0]) {
                if !agent.binaries.contains_key(bin) {
                    return Err(SupervisorError(format!(
                        "service {name}: command names {:?}, which agent.binaries does not declare",
                        argv[0]
                    )));
                }
            }
            let profiles = profiles
                .map(|list| normalize_service_profiles(&name, list))
                .transpose()?;
            validate_listen_ports(&name, &listen_ports, &declared)?;
            let ready_check = ready_check
                .as_deref()
                .map(parse_ready_check)
                .transpose()
                .map_err(|e| {
                    SupervisorError(format!("service {name}: ready_check is invalid: {e}"))
                })?;
            Ok(ServiceSpec {
                name,
                command,
                ready_check,
                restart,
                profiles,
                listen_ports,
            })
        })
        .collect()
}

fn normalize_service_profiles(
    name: &str,
    list: Vec<String>,
) -> Result<Vec<String>, SupervisorError> {
    if list.is_empty() {
        return Err(SupervisorError(format!(
            "service {name}: profiles must list at least one profile"
        )));
    }
    let mut out: Vec<String> = Vec::new();
    for raw in list {
        let Some(p) = canonical_profile(&raw) else {
            return Err(SupervisorError(format!(
                "service {name}: profile {raw:?} is not a node profile"
            )));
        };
        if !out.iter().any(|x| x == p) {
            out.push(p.to_string());
        }
    }
    Ok(out)
}

fn validate_listen_ports(
    name: &str,
    ports: &[u16],
    declared: &BTreeSet<String>,
) -> Result<(), SupervisorError> {
    if ports.is_empty() {
        return Ok(());
    }
    if ports.len() > MAX_LISTEN_PORTS {
        return Err(SupervisorError(format!(
            "service {name}: at most {MAX_LISTEN_PORTS} listen_ports"
        )));
    }
    let mut seen: BTreeSet<u16> = BTreeSet::new();
    for port in ports {
        if *port < 1024 {
            return Err(SupervisorError(format!(
                "service {name}: listen port {port} is outside 1024..=65535"
            )));
        }
        if !seen.insert(*port) {
            return Err(SupervisorError(format!(
                "service {name}: listen port {port} appears twice"
            )));
        }
    }
    let missing: Vec<&str> = [NETWORK_LISTEN_CAP, NETWORK_OUTBOUND_CAP]
        .into_iter()
        .filter(|cap| !declared.contains(*cap))
        .collect();
    if !missing.is_empty() {
        return Err(SupervisorError(format!(
            "service {name}: declares listen_ports but the plugin does not declare {}; a \
             listener needs network.listen to bind and network.outbound for the inet socket \
             families",
            missing.join(" and ")
        )));
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), SupervisorError> {
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_ok = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    if first_ok && rest_ok && name.len() <= 64 {
        Ok(())
    } else {
        Err(SupervisorError(format!(
            "service name {name:?} must be lowercase alnum plus ._- , starting with an alnum"
        )))
    }
}

/// Parse a `ready_check`: a loopback `http(s)://127.0.0.1:<port>/...` URL, or
/// an argv (POSIX quoting, never a shell).
pub fn parse_ready_check(value: &str) -> Result<ReadyCheck, String> {
    if value.chars().any(char::is_control) {
        return Err("ready_check must not contain control characters".to_string());
    }
    let text = value.trim();
    for scheme in ["http://", "https://"] {
        let Some(rest) = text.strip_prefix(scheme) else {
            continue;
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let port = authority
            .strip_prefix(&format!("{LOOPBACK_HOST}:"))
            .and_then(|p| p.parse::<u16>().ok());
        let Some(port) = port else {
            return Err(format!(
                "ready_check URL {text:?} must be http(s)://{LOOPBACK_HOST}:<port>/..."
            ));
        };
        return Ok(ReadyCheck::Http {
            url: text.to_string(),
            port,
            path: path.to_string(),
        });
    }
    let argv = split_argv(text)?;
    if argv.is_empty() {
        return Err("ready_check must not be empty".to_string());
    }
    Ok(ReadyCheck::Command(argv))
}

/// POSIX shell word splitting (quotes and backslash escapes, no expansion).
fn split_argv(text: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("unbalanced single quote".to_string()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\' | '$' | '`')) => word.push(c),
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => return Err("unbalanced double quote".to_string()),
                        },
                        Some(c) => word.push(c),
                        None => return Err("unbalanced double quote".to_string()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some(c) => word.push(c),
                    None => return Err("trailing backslash".to_string()),
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        out.push(word);
    }
    Ok(out)
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
/// trailing `-<service>` segment.
pub fn service_unit_name_for(plugin_id: &str, service_name: &str) -> String {
    format!(
        "{PLUGIN_UNIT_PREFIX}{}-{}.service",
        sanitize_unit_name(plugin_id),
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
    Ok(spec)
}

/// The readiness probe for a command `ready_check`: the argv run in the
/// plugin's install dir with the plugin's resource envelope and capability
/// sandbox.
pub fn probe_spec(
    manifest: &PluginManifest,
    argv: &[String],
    paths: &Paths,
    granted: &BTreeSet<String>,
    loopback_guard_active: bool,
) -> Result<ProbeSpec, SupervisorError> {
    let working_dir = paths.install_dir.join(&manifest.id);
    path_token("install dir", &working_dir)?;
    Ok(ProbeSpec {
        argv: argv.to_vec(),
        working_dir,
        resources: manifest
            .agent
            .as_ref()
            .map(|a| a.resources.clone())
            .unwrap_or_default(),
        sandbox_directives: sandbox_directives(granted, loopback_guard_active, &[]),
    })
}

/// GET a loopback URL; ready on a 2xx status.
pub fn probe_http(port: u16, path: &str, https: bool) -> (bool, Option<String>) {
    if https {
        // The probe is made by the agent over loopback; TLS there buys nothing
        // and this host carries no TLS client.
        return (
            false,
            Some("https ready_check is not supported; use http".to_string()),
        );
    }
    let attempt = || -> std::io::Result<u16> {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let mut stream = std::net::TcpStream::connect_timeout(&addr, HTTP_PROBE_TIMEOUT)?;
        stream.set_read_timeout(Some(HTTP_PROBE_TIMEOUT))?;
        stream.set_write_timeout(Some(HTTP_PROBE_TIMEOUT))?;
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {LOOPBACK_HOST}:{port}\r\nConnection: close\r\n\r\n"
        )?;
        let mut head = [0u8; 64];
        let n = stream.read(&mut head)?;
        let line = String::from_utf8_lossy(&head[..n]);
        line.split_whitespace()
            .nth(1)
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or_else(|| std::io::Error::other("no HTTP status line"))
    };
    match attempt() {
        Ok(code) if (200..300).contains(&code) => (true, None),
        Ok(code) => (false, Some(format!("http status {code}"))),
        Err(e) => (false, Some(format!("http probe failed: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(services: &str) -> PluginManifest {
        PluginManifest::from_yaml_text(&format!(
            "id: com.example.svc\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\n\
             agent:\n  entrypoint: agent/py/x.py\n  contributes:\n    services:\n{services}"
        ))
        .unwrap()
    }

    #[test]
    fn services_parse_both_shapes_and_refuse_a_foreign_slice() {
        let m = manifest(
            "      - bridge\n      - name: api\n        command: bin/api --port 9100\n        \
             ready_check: http://127.0.0.1:9100/health\n        restart: always\n",
        );
        let s = declared_services(&m).unwrap();
        assert_eq!(s[0].name, "bridge");
        assert_eq!(s[0].command, "bridge");
        assert_eq!(s[0].restart, "on-failure");
        assert_eq!(
            s[1].ready_check,
            Some(ReadyCheck::Http {
                url: "http://127.0.0.1:9100/health".into(),
                port: 9100,
                path: "/health".into()
            })
        );
        let bad = manifest("      - name: api\n        command: x\n        slice: system.slice\n");
        assert!(declared_services(&bad).is_err());
        let off_box = manifest(
            "      - name: api\n        command: x\n        ready_check: http://10.0.0.2:80/\n",
        );
        assert!(declared_services(&off_box).is_err());
    }

    #[test]
    fn a_service_command_cannot_smuggle_a_directive_or_a_specifier() {
        assert!(service_argv("bin/x\nExecStartPre=+/bin/sh").is_err());
        assert!(service_argv("+bin/x").is_err());
        assert!(service_argv("bin/x ; rm").is_err());
        let words: Vec<String> = service_argv("bin/x --name '%h $HOME'")
            .unwrap()
            .iter()
            .map(|w| exec_word(w))
            .collect();
        assert_eq!(words.join(" "), "bin/x --name \"%%h $$HOME\"");
    }

    fn render(m: &PluginManifest, granted: &[&str]) -> String {
        let granted: BTreeSet<String> = granted.iter().map(|s| s.to_string()).collect();
        let s = &declared_services(m).unwrap()[0];
        let paths = crate::supervisor::tests_support::fhs_paths();
        crate::backend::render_systemd(
            &build_service_spec(m, s, &paths, "workstation", &granted, true).unwrap(),
        )
    }

    #[test]
    fn a_service_unit_runs_in_the_plugin_slice_under_the_plugin_sandbox() {
        let m = manifest("      - name: api\n        command: bin/api\n");
        let unit = render(&m, &[]);
        assert!(unit.contains("Slice=ados-plugins.slice"));
        assert!(unit.contains("WorkingDirectory=/var/ados/plugins/com.example.svc"));
        assert!(unit.contains("ExecStart=bin/api"));
        assert!(unit.contains("User=ados"));
        assert!(unit.contains("NoNewPrivileges=yes"));
        assert_eq!(
            service_unit_name_for("com.example.svc", "api"),
            "ados-plugin-com-example-svc-api.service"
        );
    }

    #[test]
    fn a_service_shares_the_plugins_identity_on_the_host_socket() {
        // A declared service reaches the host SDK exactly as the main process
        // does: same socket, same token file, same socket-dir bind.
        let unit = render(
            &manifest("      - name: api\n        command: bin/api\n"),
            &[],
        );
        assert!(unit.contains(
            "Environment=ADOS_PLUGIN_SOCKET=/run/ados/plugins/com.example.svc/host.sock"
        ));
        assert!(unit.contains("EnvironmentFile=-/run/ados/plugins/com.example.svc.token.env"));
        assert!(unit.contains("BindReadOnlyPaths=/run/ados/plugins/com.example.svc\n"));
        assert!(unit.contains("Environment=ADOS_NODE_PROFILE=workstation\n"));
    }

    fn listener(perms: &str) -> String {
        format!(
            "id: com.example.svc\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\n\
             agent:\n  entrypoint: agent/py/x.py\n  permissions: [{perms}]\n  contributes:\n    \
             services:\n      - name: node\n        command: bin/node\n        \
             listen_ports: [8092]\n        profiles: [workstation, ground_station]\n"
        )
    }

    #[test]
    fn a_listener_binds_only_its_declared_ports_and_only_when_granted() {
        let m =
            PluginManifest::from_yaml_text(&listener("network.listen, network.outbound")).unwrap();
        let s = &declared_services(&m).unwrap()[0];
        assert_eq!(s.listen_ports, vec![8092]);
        assert_eq!(
            s.profiles,
            Some(vec![
                "workstation".to_string(),
                "ground-station".to_string()
            ])
        );
        assert!(s.applies_to("workstation") && !s.applies_to("drone"));

        let ungranted = render(&m, &["network.outbound"]);
        assert!(ungranted.contains("SocketBindDeny=any"));
        assert!(!ungranted.contains("SocketBindAllow="));
        let granted = render(&m, &["network.outbound", "network.listen"]);
        assert!(
            granted.contains("SocketBindAllow=tcp:8092\nSocketBindDeny=any"),
            "{granted}"
        );
    }

    #[test]
    fn listen_ports_without_the_listen_and_network_permissions_are_refused() {
        for perms in ["", "network.listen", "network.outbound"] {
            let m = PluginManifest::from_yaml_text(&listener(perms)).unwrap();
            let err = declared_services(&m).unwrap_err();
            assert!(
                err.0.contains("declares listen_ports"),
                "{perms:?}: {}",
                err.0
            );
        }
        let too_low = listener("network.listen, network.outbound").replace("[8092]", "[80]");
        let err =
            declared_services(&PluginManifest::from_yaml_text(&too_low).unwrap()).unwrap_err();
        assert!(err.0.contains("outside 1024..=65535"), "{}", err.0);
        let too_many = listener("network.listen, network.outbound")
            .replace("[8092]", "[2001, 2002, 2003, 2004, 2005]");
        assert!(declared_services(&PluginManifest::from_yaml_text(&too_many).unwrap()).is_err());
    }

    #[test]
    fn an_http_probe_reads_the_status_code() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            for reply in [
                "HTTP/1.1 204 No Content\r\n\r\n",
                "HTTP/1.1 503 Busy\r\n\r\n",
            ] {
                let (mut s, _) = listener.accept().unwrap();
                // Read the whole request head before answering: replying and
                // closing mid-request leaves the client writing into a closed
                // socket (EPIPE), which is not the answer under test.
                let mut head = Vec::new();
                let mut buf = [0u8; 256];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                s.write_all(reply.as_bytes()).unwrap();
            }
        });
        assert_eq!(probe_http(port, "/health", false), (true, None));
        assert_eq!(
            probe_http(port, "/health", false),
            (false, Some("http status 503".to_string()))
        );
        server.join().unwrap();
    }
}
