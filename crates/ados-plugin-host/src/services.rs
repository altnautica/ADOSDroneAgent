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
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::errors::SupervisorError;
use crate::manifest::{AgentIsolation, PluginManifest};
use crate::sandbox::sandbox_directives;
use crate::systemd::{
    sanitize_unit_name, service_log_path_for, HARDENING_DIRECTIVES, PLUGIN_SLICE_NAME,
    PLUGIN_UNIT_DIR, PLUGIN_UNIT_PREFIX,
};

/// How long one command readiness probe may run before systemd stops it.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long an HTTP readiness probe may take.
const HTTP_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The only host an HTTP readiness probe may name.
const LOOPBACK_HOST: &str = "127.0.0.1";

/// One declared service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSpec {
    pub name: String,
    pub command: String,
    pub ready_check: Option<ReadyCheck>,
    pub restart: String,
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
    raw.into_iter()
        .map(|r| {
            let (name, command, ready_check, restart, slice) = match r {
                RawService::Bare(s) => (s.clone(), s, None, default_restart(), default_slice()),
                RawService::Full {
                    name,
                    command,
                    ready_check,
                    restart,
                    slice,
                } => (name, command, ready_check, restart, slice),
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
            })
        })
        .collect()
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

/// Render a plugin-authored `command` as one `ExecStart=` value: split as an
/// argv and re-emitted in systemd quoting with `%` and `$` doubled, so no
/// specifier or variable expands. Refuses a control character (a newline would
/// start a new directive), an empty command, a first word carrying a systemd
/// prefix character, and a lone `;` word (systemd's command separator).
pub fn exec_start_value(command: &str) -> Result<String, SupervisorError> {
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
    Ok(argv
        .iter()
        .map(|w| exec_word(w))
        .collect::<Vec<_>>()
        .join(" "))
}

fn exec_word(word: &str) -> String {
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

/// The unit file path for a declared service under `unit_dir`.
pub fn service_unit_path_for(
    plugin_id: &str,
    service_name: &str,
    unit_dir: Option<&Path>,
) -> PathBuf {
    unit_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(PLUGIN_UNIT_DIR))
        .join(service_unit_name_for(plugin_id, service_name))
}

/// Render the unit for one declared service: its own `ExecStart` in the
/// plugin's install dir, the shared plugin slice, the main unit's hardening and
/// resource envelope, and the plugin's capability sandbox.
pub fn render_service_unit(
    manifest: &PluginManifest,
    service: &ServiceSpec,
    install_dir: &Path,
    granted: &BTreeSet<String>,
    loopback_guard_active: bool,
) -> Result<String, SupervisorError> {
    let Some(agent) = manifest.agent.as_ref() else {
        return Err(SupervisorError(format!(
            "plugin {} has no agent half; no service unit needed",
            manifest.id
        )));
    };
    let res = &agent.resources;
    let working_dir = install_dir.join(&manifest.id);
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
    Ok(format!(
        "\
[Unit]
Description=ADOS plugin {plugin_id} service {service_name}
After=ados-supervisor.service
PartOf=ados-supervisor.service
StartLimitIntervalSec=0

[Service]
Slice={slice}
Type=simple
WorkingDirectory={working_dir}
ExecStart={exec_start}
Restart={restart}
RestartSec=2s
MemoryMax={max_ram_mb}M
CPUQuota={max_cpu_percent}%
TasksMax={max_pids}
StandardOutput=append:{log_path}
StandardError=append:{log_path}
User=ados
Group=ados
{hardening}
# ---- capability sandbox (re-rendered on every grant/revoke) ----
{sandbox}

[Install]
WantedBy=ados-supervisor.service
",
        plugin_id = manifest.id,
        service_name = service.name,
        slice = PLUGIN_SLICE_NAME,
        working_dir = working_dir.display(),
        exec_start = exec_start_value(&service.command)?,
        restart = service.restart,
        max_ram_mb = res.max_ram_mb,
        max_cpu_percent = res.max_cpu_percent,
        max_pids = res.max_pids,
        log_path = service_log_path_for(&manifest.id, &service.name),
        hardening = HARDENING_DIRECTIVES.join("\n"),
        sandbox = sandbox_directives(granted, loopback_guard_active).join("\n"),
    ))
}

/// The `systemd-run` argv that runs one command readiness probe as a transient
/// unit with exactly what the plugin's services get: the `ados` user, the
/// hardening, the resource envelope, the capability sandbox and the plugin
/// slice. The argv is passed through as separate words; no shell sees it.
pub fn probe_command(
    manifest: &PluginManifest,
    argv: &[String],
    install_dir: &Path,
    granted: &BTreeSet<String>,
    loopback_guard_active: bool,
) -> Vec<String> {
    let mut out: Vec<String> = [
        "systemd-run",
        "--quiet",
        "--wait",
        "--pipe",
        "--collect",
        "--uid=ados",
        "--gid=ados",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    out.push(format!("--slice={PLUGIN_SLICE_NAME}"));
    out.push(format!(
        "--working-directory={}",
        install_dir.join(&manifest.id).display()
    ));
    let mut properties: Vec<String> = HARDENING_DIRECTIVES.iter().map(|s| s.to_string()).collect();
    if let Some(agent) = manifest.agent.as_ref() {
        let res = &agent.resources;
        properties.push(format!("MemoryMax={}M", res.max_ram_mb));
        properties.push(format!("CPUQuota={}%", res.max_cpu_percent));
        properties.push(format!("TasksMax={}", res.max_pids));
    }
    properties.push(format!("RuntimeMaxSec={}", PROBE_TIMEOUT.as_secs()));
    properties.extend(sandbox_directives(granted, loopback_guard_active));
    out.extend(properties.into_iter().map(|p| format!("--property={p}")));
    out.push("--".to_string());
    out.extend(argv.iter().cloned());
    out
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
        assert!(exec_start_value("bin/x\nExecStartPre=+/bin/sh").is_err());
        assert!(exec_start_value("+bin/x").is_err());
        assert!(exec_start_value("bin/x ; rm").is_err());
        assert_eq!(
            exec_start_value("bin/x --name '%h $HOME'").unwrap(),
            "bin/x --name \"%%h $$HOME\""
        );
    }

    #[test]
    fn a_service_unit_runs_in_the_plugin_slice_under_the_plugin_sandbox() {
        let m = manifest("      - name: api\n        command: bin/api\n");
        let s = &declared_services(&m).unwrap()[0];
        let unit = render_service_unit(
            &m,
            s,
            Path::new("/var/ados/plugins"),
            &BTreeSet::new(),
            false,
        )
        .unwrap();
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
