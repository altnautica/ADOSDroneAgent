//! Parsing and validating declared plugin services.

use super::*;

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

pub(super) fn normalize_service_profiles(
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

pub(super) fn validate_listen_ports(
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

pub(super) fn validate_name(name: &str) -> Result<(), SupervisorError> {
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
