//! The LoRa lane surface.
//!
//! - **`GET /api/v1/lora`**: the lane's state sidecar plus `present: true`;
//!   when the lane is not running (absent or stale sidecar) the
//!   `present: false` shape with empty values, never a 404.
//! - **`GET /api/v1/lora/regions`**: the compiled band-plan table, each row
//!   flagged with whether the fitted module meets it (null when no module is
//!   known).
//! - **`GET /api/v1/lora/beacons`**: beacons heard, newest first.
//! - **`PUT /api/v1/lora`**: `{"region": ".." | null}` and/or
//!   `{"erp_mw", "freq_mhz", "sf"}`. Every value is checked against the plan it
//!   would run under (`400 E_LORA_OUT_OF_PLAN`), persisted to the `lora:`
//!   block, and the lane is reloaded through the config side effects. A region
//!   change is an admin-scope write ([`crate::mcp`]) and is refused while the
//!   vehicle is armed ([`crate::armed_guard`]).

use std::path::Path;
use std::time::{Duration, SystemTime};

use ados_config::side_effects;
use ados_lora::airtime::Modem;
use ados_lora::band::ModuleCaps;
use ados_lora::config::LoraConfig;
use ados_lora::governor::power::Antenna;
use ados_lora::regions;
use ados_lora::status::{absent_body, regions_body};
use ados_lora::validate::{self, Proposed};
use ados_protocol::sidecar::{read_fresh_at, Reading};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

use crate::config_store::{section_path, update_config};
use crate::routes::detail;
use crate::state::AppState;

fn window(id: &str) -> Option<Duration> {
    ados_protocol::paths::max_age(id)
}

/// The fresh lane sidecar, or `None` when the lane is not reporting.
fn lane_sidecar(run_dir: &Path, now: SystemTime) -> Option<Map<String, Value>> {
    let path = run_dir.join(ados_protocol::paths::LORA_JSON);
    match read_fresh_at::<Map<String, Value>>(&path, window("lora"), now) {
        Reading::Fresh(doc) => Some(doc),
        _ => None,
    }
}

/// The `GET /api/v1/lora` body from the sidecar (or its absence).
pub(crate) fn status_body(run_dir: &Path, now: SystemTime) -> Value {
    match lane_sidecar(run_dir, now) {
        Some(mut doc) => {
            let version = doc.remove("v").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
            let expected = ados_protocol::contracts::sidecar_version("lora").unwrap_or(0);
            ados_protocol::sidecar::check_sidecar_version("lora", version, expected);
            doc.insert("present".into(), json!(true));
            Value::Object(doc)
        }
        None => absent_body(),
    }
}

/// The capability flag: the lane's unit runs on this profile (wire or
/// underscore form) and the `lora-enabled` marker beside the config is set.
pub(crate) fn lora_present(config_path: &Path, profile: &str) -> bool {
    let profile = profile.replace('-', "_");
    let on_profile =
        ados_protocol::units::find("ados-lora").is_some_and(|u| u.on_profile(&profile));
    let etc_dir = config_path.parent().unwrap_or(Path::new("."));
    on_profile && etc_dir.join("lora-enabled").exists()
}

/// The fitted module, from the running lane's `module_band`.
fn module_caps(run_dir: &Path, now: SystemTime) -> Option<ModuleCaps> {
    let doc = lane_sidecar(run_dir, now)?;
    ModuleCaps::from_label(doc.get("module_band")?.as_str()?)
}

pub(crate) fn regions_response(run_dir: &Path, config_path: &Path, now: SystemTime) -> Value {
    let bw = lane_sidecar(run_dir, now)
        .and_then(|d| d.get("channel")?.get("bw_khz")?.as_u64())
        .map(|b| b as u32)
        .unwrap_or_else(|| load_lora(config_path).map_or(125, |c| c.bw_khz));
    regions_body(module_caps(run_dir, now).as_ref(), bw)
}

pub(crate) fn beacons_body(run_dir: &Path, now: SystemTime) -> Value {
    let path = run_dir.join(ados_protocol::paths::LORA_BEACONS_JSON);
    let beacons = match read_fresh_at::<Value>(&path, window("lora-beacons"), now) {
        Reading::Fresh(doc) => doc.get("beacons").cloned().unwrap_or_else(|| json!([])),
        _ => json!([]),
    };
    json!({ "beacons": beacons })
}

pub async fn get_lora(State(state): State<AppState>) -> Json<Value> {
    Json(status_body(&state.run_dir, SystemTime::now()))
}

pub async fn get_lora_regions(State(state): State<AppState>) -> Json<Value> {
    Json(regions_response(
        &state.run_dir,
        &state.pairing_paths.config,
        SystemTime::now(),
    ))
}

pub async fn get_lora_beacons(State(state): State<AppState>) -> Json<Value> {
    Json(beacons_body(&state.run_dir, SystemTime::now()))
}

fn out_of_plan(detail: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": "E_LORA_OUT_OF_PLAN", "detail": detail.into()})),
    )
        .into_response()
}

fn load_lora(config_path: &Path) -> Result<LoraConfig, String> {
    ados_config::schema::load_slice(config_path, "/lora")
}

fn country(config_path: &Path) -> Option<String> {
    ados_config::schema::load_slice::<Value>(config_path, "/network/regulatory")
        .ok()?
        .get("region")?
        .as_str()
        .map(str::to_string)
}

/// The validated change: the YAML fields to set under `lora`.
type Change = Vec<(&'static str, Value)>;

/// Validate a `PUT` body against the plan it would run under.
pub(crate) fn plan_change(
    body: &Map<String, Value>,
    cfg: &LoraConfig,
    country: Option<&str>,
    caps: Option<&ModuleCaps>,
) -> Result<Change, String> {
    let mut change = Change::new();
    let region = match body.get("region") {
        None => cfg.region.clone(),
        Some(Value::Null) => {
            change.push(("region", Value::Null));
            None
        }
        Some(Value::String(code)) => {
            let code = code.trim().to_ascii_uppercase();
            if regions::lookup(&code).is_none() {
                return Err(format!("region {code:?} is not in the compiled table"));
            }
            change.push(("region", json!(code)));
            Some(code)
        }
        Some(_) => return Err("region must be a string or null".into()),
    };
    let num = |k: &str| -> Result<Option<f64>, String> {
        match body.get(k) {
            None => Ok(None),
            Some(v) => v
                .as_f64()
                .map(Some)
                .ok_or_else(|| format!("{k} must be a number")),
        }
    };
    let freq = num("freq_mhz")?;
    let erp = num("erp_mw")?;
    let sf = num("sf")?;
    if let Some(f) = freq {
        change.push(("freq_mhz", json!(f)));
    }
    if let Some(e) = erp {
        change.push(("erp_mw", json!(e)));
    }
    if let Some(s) = sf {
        if s.fract() != 0.0 {
            return Err("sf must be an integer".into());
        }
        change.push(("sf", json!(s as u8)));
    }
    let resolved = regions::resolve(region.as_deref(), country);
    let touches_radio = freq.is_some() || erp.is_some() || sf.is_some();
    let Some(r) = resolved else {
        return if touches_radio {
            Err("no region resolves (set lora.region or the country setting)".into())
        } else {
            Ok(change)
        };
    };
    let proposed = Proposed {
        freq_mhz: freq.or(cfg.freq_mhz),
        erp_mw: erp.or(cfg.erp_mw),
        modem: Modem {
            sf: sf.map_or(cfg.sf, |s| s as u8),
            ..cfg.modem()
        },
        transport: cfg.transport.clone(),
        antenna: Antenna {
            gain_dbi: cfg.antenna_gain_dbi,
            cable_loss_db: cfg.cable_loss_db,
        },
    };
    if touches_radio || body.contains_key("region") {
        validate::check(r.plan, caps, &proposed)?;
    }
    Ok(change)
}

/// Persist the change and mirror it onto the lane (marker, reload). Returns
/// the unit commands to run.
fn persist(config_path: &Path, change: Change) -> Result<Vec<Vec<String>>, String> {
    use serde_norway::Value as Yaml;
    let written = update_config(config_path, |root| {
        let before = root.clone();
        let lora = section_path(root, &["lora"]);
        for (k, v) in change {
            let y = serde_norway::to_value(&v).map_err(|e| e.to_string())?;
            lora.insert(Yaml::String(k.to_string()), y);
        }
        Ok((before, root.clone()))
    })
    .map_err(|e| e.to_string())?;
    if !written.wrote {
        return Ok(Vec::new());
    }
    let (before, after) = written.value;
    let as_json = |m| serde_json::to_value(Yaml::Mapping(m));
    let (Ok(old), Ok(new)) = (as_json(before), as_json(after)) else {
        return Ok(Vec::new());
    };
    let issued = std::cell::RefCell::new(Vec::new());
    let run = |program: &str, args: &[&str]| {
        let mut cmd = vec![program.to_string()];
        cmd.extend(args.iter().map(|a| a.to_string()));
        issued.borrow_mut().push(cmd);
        true
    };
    let etc_dir = config_path.parent().unwrap_or(Path::new("."));
    let report = side_effects::apply(&old, &new, etc_dir, &run);
    if !report.failures.is_empty() {
        tracing::warn!(failures = ?report.failures, "lora config side effects partly failed");
    }
    Ok(issued.into_inner())
}

pub async fn put_lora(State(state): State<AppState>, body: Bytes) -> Response {
    let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(&body) else {
        return detail(StatusCode::BAD_REQUEST, "body must be a JSON object");
    };
    let config_path = state.pairing_paths.config.clone();
    let cfg = match load_lora(&config_path) {
        Ok(c) => c,
        Err(e) => return detail(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let caps = module_caps(&state.run_dir, SystemTime::now());
    let change = match plan_change(&body, &cfg, country(&config_path).as_deref(), caps.as_ref()) {
        Ok(c) => c,
        Err(e) => return out_of_plan(e),
    };
    let path = config_path.clone();
    let commands = match tokio::task::spawn_blocking(move || persist(&path, change)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return detail(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => return detail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    for cmd in commands {
        let Some((program, args)) = cmd.split_first() else {
            continue;
        };
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        if !crate::probe::status_only(program, &args, crate::probe::PROBE_TIMEOUT).await {
            tracing::warn!(command = %cmd.join(" "), "lora side-effect command failed");
        }
    }
    Json(status_body(&state.run_dir, SystemTime::now())).into_response()
}

#[cfg(test)]
mod tests;
