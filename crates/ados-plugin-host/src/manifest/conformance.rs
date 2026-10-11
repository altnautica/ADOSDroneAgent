//! The strict shape rules of the schema-source model, checked on the raw YAML
//! before the lifecycle subset is read: no unknown key in any block, the
//! required fields present, and the informational fields
//! typed and bounded. The shared conformance corpus pins the agreement.
//!
//! `name` stays optional here: the install path has always accepted a
//! manifest without one (the lifecycle names a plugin by its id), so the
//! corpus carries no missing-name case.

use super::{validate_entrypoint, ManifestError, RISK_LEVELS};
use serde_norway::Value;

const TOP: &[&str] = &[
    "schema_version",
    "id",
    "version",
    "name",
    "description",
    "author",
    "homepage",
    "license",
    "risk",
    "compatibility",
    "agent",
    "gcs",
    "description_long",
    "features",
    "hardware_requirements",
    "resource_impact",
    "required_fc_parameters",
    "telemetry_fields",
    "documentation_url",
    "screenshots",
    "extra",
];
const COMPATIBILITY: &[&str] = &[
    "ados_version",
    "gcs_version",
    "supported_boards",
    "min_tier",
];
const AGENT: &[&str] = &[
    "entrypoint",
    "isolation",
    "runtime",
    "permissions",
    "resources",
    "contributes",
    "mavlink_components",
    "contains_vendor_binary",
    "test_fixtures",
    "vendor_attribution",
    "subprocess_spawn",
    "per_drone_config",
    "target_profiles",
    "binaries",
    "payloads",
    "http",
    "declared_capabilities",
];
const PERMISSION: &[&str] = &["id", "required", "degraded_behavior"];
const RESOURCES: &[&str] = &["class", "max_ram_mb", "max_cpu_percent", "max_pids"];
const AGENT_CONTRIBUTES: &[&str] = &[
    "services",
    "drivers",
    "vision",
    "tools",
    "resources",
    "prompts",
    "shared_topics",
];
const VISION: &[&str] = &["behaviors", "models", "detectors"];
const SERVICE: &[&str] = &[
    "name",
    "command",
    "ready_check",
    "restart",
    "slice",
    "profiles",
    "listen_ports",
];
const MAVLINK_COMPONENT: &[&str] = &["component_id", "component_kind", "sub_id"];
const COMPONENT_KINDS: &[&str] = &[
    "camera",
    "gimbal",
    "payload",
    "peripheral",
    "generic",
    "vio",
];
const PAYLOAD: &[&str] = &[
    "path",
    "source",
    "sha256",
    "size_bytes",
    "profiles",
    "arch_os",
];
const DECLARED_CAPABILITY: &[&str] = &["id", "risk", "description"];
const SHARED_TOPIC: &[&str] = &["topic", "subscribe_capability"];
const GCS: &[&str] = &[
    "entrypoint",
    "isolation",
    "permissions",
    "contributes",
    "locales",
];
const GCS_CONTRIBUTES: &[&str] = &[
    "panels",
    "overlays",
    "notifications",
    "smart_functions",
    "tabs",
    "parameters",
    "models",
    "skills",
    "target_actions",
    "tools",
    "resources",
    "prompts",
    "settings",
    "map_overlays",
    "mission_templates",
    "agent_pages",
    "node_surfaces",
];
const HARDWARE: &[&str] = &["cameras", "fc_firmware", "boards", "optional"];
const IMPACT: &[&str] = &[
    "cpu_percent_peak",
    "ram_mb",
    "pids",
    "startup_time_seconds",
    "output_rate_hz",
];
const FC_PARAMETERS: &[&str] = &["ardupilot", "px4", "inav"];
const FC_PARAMETER: &[&str] = &["param", "note", "value"];
const SCREENSHOT: &[&str] = &["url", "caption"];

fn err(msg: String) -> Result<(), ManifestError> {
    Err(ManifestError(msg))
}

/// `v` is a mapping whose keys are all in `allowed`. A null or absent
/// block passes (the typed parse decides whether it is required).
fn keys(field: &str, v: Option<&Value>, allowed: &[&str]) -> Result<(), ManifestError> {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let Some(map) = v.as_mapping() else {
        return err(format!("{field} must be a mapping"));
    };
    for key in map.keys() {
        match key.as_str() {
            Some(k) if allowed.contains(&k) => {}
            _ => return err(format!("{field} has an unknown field {key:?}")),
        }
    }
    Ok(())
}

/// Each element of the list at `v` (if any) as a mapping.
fn each<'a>(field: &str, v: Option<&'a Value>) -> Result<Vec<&'a Value>, ManifestError> {
    match v.filter(|v| !v.is_null()) {
        None => Ok(Vec::new()),
        Some(Value::Sequence(items)) => Ok(items.iter().collect()),
        Some(_) => Err(ManifestError(format!("{field} must be a list"))),
    }
}

fn strings(field: &str, v: Option<&Value>) -> Result<(), ManifestError> {
    for item in each(field, v)? {
        if item.as_str().is_none() {
            return err(format!("{field} entries must be strings"));
        }
    }
    Ok(())
}

fn text(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).map(str::trim)
}

fn opt_string(field: &str, v: Option<&Value>) -> Result<(), ManifestError> {
    match v {
        None | Some(Value::Null) | Some(Value::String(_)) => Ok(()),
        Some(_) => err(format!("{field} must be a string")),
    }
}

fn opt_bool(field: &str, v: Option<&Value>) -> Result<(), ManifestError> {
    match v {
        None | Some(Value::Bool(_)) => Ok(()),
        Some(_) => err(format!("{field} must be a boolean")),
    }
}

/// A number in `lo..=hi` (`lo` exclusive when `open_lo`), or absent/null.
fn opt_number(
    field: &str,
    v: Option<&Value>,
    lo: f64,
    open_lo: bool,
    hi: f64,
) -> Result<(), ManifestError> {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let Some(n) = v.as_f64() else {
        return err(format!("{field} must be a number"));
    };
    let above = if open_lo { n > lo } else { n >= lo };
    if above && n <= hi {
        Ok(())
    } else {
        err(format!("{field} {n} is out of range"))
    }
}

fn permissions(field: &str, v: Option<&Value>) -> Result<(), ManifestError> {
    for p in each(field, v)? {
        if p.is_mapping() {
            keys(field, Some(p), PERMISSION)?;
        } else if p.as_str().is_none() {
            return err(format!("{field} entries must be a string or an object"));
        }
    }
    Ok(())
}

pub(super) fn check(root: &Value) -> Result<(), ManifestError> {
    keys("manifest", Some(root), TOP)?;
    opt_string("name", root.get("name"))?;
    if let Some(v) = root.get("schema_version") {
        if !v.as_i64().is_some_and(|n| (1..=3).contains(&n)) {
            return err(format!("schema_version {v:?} must be an integer 1..=3"));
        }
    }
    if let Some(url) = root.get("documentation_url").filter(|v| !v.is_null()) {
        if !url
            .as_str()
            .is_some_and(|u| u.trim().starts_with("https://"))
        {
            return err(format!("documentation_url must use https://, got {url:?}"));
        }
    }
    for field in [
        "description",
        "author",
        "license",
        "homepage",
        "description_long",
    ] {
        opt_string(field, root.get(field))?;
    }
    strings("features", root.get("features"))?;
    strings("telemetry_fields", root.get("telemetry_fields"))?;
    // `extra` is free-form: only its shape is checked.
    if root
        .get("extra")
        .is_some_and(|v| !v.is_null() && !v.is_mapping())
    {
        return err("extra must be a mapping".to_string());
    }
    for shot in each("screenshots", root.get("screenshots"))? {
        keys("screenshots[]", Some(shot), SCREENSHOT)?;
        if !text(shot.get("url")).is_some_and(|u| !u.is_empty()) {
            return err("screenshots[].url must be a non-empty string".to_string());
        }
    }
    let hardware = root.get("hardware_requirements");
    keys("hardware_requirements", hardware, HARDWARE)?;
    if let Some(h) = hardware {
        opt_string("hardware_requirements.cameras", h.get("cameras"))?;
        opt_string("hardware_requirements.fc_firmware", h.get("fc_firmware"))?;
        strings("hardware_requirements.boards", h.get("boards"))?;
        strings("hardware_requirements.optional", h.get("optional"))?;
    }
    let impact = root.get("resource_impact");
    keys("resource_impact", impact, IMPACT)?;
    if let Some(i) = impact {
        opt_number(
            "resource_impact.cpu_percent_peak",
            i.get("cpu_percent_peak"),
            0.0,
            false,
            1000.0,
        )?;
        opt_number(
            "resource_impact.ram_mb",
            i.get("ram_mb"),
            0.0,
            true,
            f64::MAX,
        )?;
        opt_number("resource_impact.pids", i.get("pids"), 0.0, true, f64::MAX)?;
        opt_number(
            "resource_impact.startup_time_seconds",
            i.get("startup_time_seconds"),
            0.0,
            true,
            f64::MAX,
        )?;
        opt_number(
            "resource_impact.output_rate_hz",
            i.get("output_rate_hz"),
            0.0,
            true,
            10000.0,
        )?;
    }
    let fc = root.get("required_fc_parameters");
    keys("required_fc_parameters", fc, FC_PARAMETERS)?;
    for firmware in FC_PARAMETERS {
        let field = format!("required_fc_parameters.{firmware}");
        for p in each(&field, fc.and_then(|f| f.get(*firmware)))? {
            keys(&field, Some(p), FC_PARAMETER)?;
            if !text(p.get("param")).is_some_and(|s| !s.is_empty()) {
                return err(format!("{field}[].param must be a non-empty string"));
            }
        }
    }
    let compat = root.get("compatibility");
    keys("compatibility", compat, COMPATIBILITY)?;
    if !text(compat.and_then(|c| c.get("ados_version"))).is_some_and(|s| !s.is_empty()) {
        return err("compatibility.ados_version must be a non-empty string".to_string());
    }
    if let Some(agent) = root.get("agent").filter(|v| !v.is_null()) {
        check_agent(agent)?;
    }
    if let Some(gcs) = root.get("gcs").filter(|v| !v.is_null()) {
        keys("gcs", Some(gcs), GCS)?;
        permissions("gcs.permissions", gcs.get("permissions"))?;
        strings("gcs.locales", gcs.get("locales"))?;
        let contributes = gcs.get("contributes");
        keys("gcs.contributes", contributes, GCS_CONTRIBUTES)?;
        for key in GCS_CONTRIBUTES {
            let field = format!("gcs.contributes.{key}");
            for item in each(&field, contributes.and_then(|c| c.get(*key)))? {
                if !item.is_mapping() {
                    return err(format!("{field} entries must be objects"));
                }
            }
        }
    }
    Ok(())
}

fn check_agent(agent: &Value) -> Result<(), ManifestError> {
    keys("agent", Some(agent), AGENT)?;
    permissions("agent.permissions", agent.get("permissions"))?;
    keys("agent.resources", agent.get("resources"), RESOURCES)?;
    opt_bool("agent.per_drone_config", agent.get("per_drone_config"))?;
    opt_bool("agent.http", agent.get("http"))?;
    strings("agent.subprocess_spawn", agent.get("subprocess_spawn"))?;
    for c in each("agent.mavlink_components", agent.get("mavlink_components"))? {
        keys("agent.mavlink_components[]", Some(c), MAVLINK_COMPONENT)?;
        if !c
            .get("component_id")
            .and_then(Value::as_u64)
            .is_some_and(|n| n <= 255)
        {
            return err("agent.mavlink_components[].component_id must be 0..=255".to_string());
        }
        if !text(c.get("component_kind")).is_some_and(|k| COMPONENT_KINDS.contains(&k)) {
            return err(format!(
                "agent.mavlink_components[].component_kind must be one of {COMPONENT_KINDS:?}"
            ));
        }
        if let Some(sub) = c.get("sub_id").filter(|v| !v.is_null()) {
            if !sub.as_u64().is_some_and(|n| n <= 255) {
                return err("agent.mavlink_components[].sub_id must be 0..=255".to_string());
            }
        }
    }
    if let Some(fixtures) = agent.get("test_fixtures").filter(|v| !v.is_null()) {
        let Some(map) = fixtures.as_mapping() else {
            return err("agent.test_fixtures must be a mapping".to_string());
        };
        for (name, path) in map {
            let (Some(name), Some(path)) = (name.as_str(), path.as_str()) else {
                return err("agent.test_fixtures must map names to paths".to_string());
            };
            if name.is_empty() || path.is_empty() {
                return err("agent.test_fixtures names and paths must be non-empty".to_string());
            }
            validate_entrypoint(&format!("agent.test_fixtures[{name}]"), path)?;
        }
    }
    for record in each("agent.vendor_attribution", agent.get("vendor_attribution"))? {
        if !record.is_mapping() || !text(record.get("license")).is_some_and(|l| !l.is_empty()) {
            return err(
                "agent.vendor_attribution[].license must be a non-empty string".to_string(),
            );
        }
    }
    for p in each("agent.payloads", agent.get("payloads"))? {
        keys("agent.payloads[]", Some(p), PAYLOAD)?;
    }
    for c in each(
        "agent.declared_capabilities",
        agent.get("declared_capabilities"),
    )? {
        keys(
            "agent.declared_capabilities[]",
            Some(c),
            DECLARED_CAPABILITY,
        )?;
        if !text(c.get("risk")).is_some_and(|r| RISK_LEVELS.contains(&r)) {
            return err(format!(
                "agent.declared_capabilities[].risk must be one of {RISK_LEVELS:?}"
            ));
        }
    }
    let contributes = agent.get("contributes");
    keys("agent.contributes", contributes, AGENT_CONTRIBUTES)?;
    let Some(contributes) = contributes else {
        return Ok(());
    };
    keys(
        "agent.contributes.vision",
        contributes.get("vision"),
        VISION,
    )?;
    for s in each("agent.contributes.services", contributes.get("services"))? {
        if s.is_mapping() {
            keys("agent.contributes.services[]", Some(s), SERVICE)?;
        }
    }
    for t in each(
        "agent.contributes.shared_topics",
        contributes.get("shared_topics"),
    )? {
        keys("agent.contributes.shared_topics[]", Some(t), SHARED_TOPIC)?;
    }
    for key in ["drivers", "tools", "resources", "prompts"] {
        let field = format!("agent.contributes.{key}");
        for item in each(&field, contributes.get(key))? {
            if !item.is_mapping() {
                return err(format!("{field} entries must be objects"));
            }
        }
    }
    Ok(())
}
