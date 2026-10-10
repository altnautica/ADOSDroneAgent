//! Dump every shipped systemd unit as JSON, for `scripts/emit-reference.py`.
//!
//! Each row joins the unit file (`data/systemd/*.service`) with the facts the
//! code holds about it: the supervisor registry's category and profile gate,
//! whether the REST restart route may touch it, and whether a restart is
//! refused while armed because it drops a link.
//!
//! ```text
//! cargo run --quiet -p ados-control --example units
//! ```

use std::path::PathBuf;

use ados_control::armed_guard::is_guarded;
use ados_control::routes::service_control::ALLOWED_UNITS;
use ados_supervisor::registry::{Category, SERVICE_REGISTRY};
use serde_json::{json, Value};

/// Every agent profile, used for a registry entry with no profile gate.
const ALL_PROFILES: &[&str] = &["drone", "ground_station", "workstation", "compute"];

fn directive<'a>(body: &'a str, key: &str) -> impl Iterator<Item = &'a str> + 'a {
    let prefix = format!("{key}=");
    body.lines()
        .filter_map(move |l| l.trim().strip_prefix(prefix.as_str()).map(str::trim))
}

fn unit_row(name: &str, body: &str) -> Value {
    let def = SERVICE_REGISTRY.iter().find(|d| d.name == name);
    let category = def.map(|d| match d.category {
        Category::Core => "core",
        Category::Hardware => "hardware",
        Category::OnDemand => "on_demand",
    });
    let profiles: Option<Vec<&str>> = def.map(|d| match d.profile_gate {
        Some(gate) => gate.split('|').collect(),
        None => ALL_PROFILES.to_vec(),
    });
    json!({
        "name": name,
        "description": directive(body, "Description").next(),
        "category": category,
        "profiles": profiles,
        "roles": [],
        "install": null,
        "start_on_install": null,
        "api_restartable": ALLOWED_UNITS.contains(&name),
        "drops_link": is_guarded("POST", &format!("/api/services/{name}/restart")),
        "memory_max": directive(body, "MemoryMax").next(),
        "condition_paths": directive(body, "ConditionPathExists").collect::<Vec<_>>(),
    })
}

fn main() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/systemd");
    let mut units: Vec<(String, String)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "service"))
        .map(|p| {
            let name = p.file_stem().unwrap().to_string_lossy().into_owned();
            let body =
                std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
            (name, body)
        })
        .collect();
    units.sort();
    let rows: Vec<Value> = units.iter().map(|(n, b)| unit_row(n, b)).collect();
    println!("{}", json!({ "schema": 1, "units": rows }));
}
