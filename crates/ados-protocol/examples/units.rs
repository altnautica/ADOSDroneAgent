//! Dump every shipped systemd unit as JSON, for `scripts/emit-reference.py`.
//!
//! Each row joins a [`ados_protocol::units::UNITS`] entry with its unit file
//! (`data/systemd/<name>`): the description, memory ceiling and start
//! conditions come from the file, everything else from the table.
//!
//! ```text
//! cargo run --quiet -p ados-protocol --example units
//! ```

use std::path::PathBuf;

use ados_protocol::units::{UnitSpec, ALL_PROFILES, UNITS};
use serde_json::{json, Value};

fn directive<'a>(body: &'a str, key: &str) -> impl Iterator<Item = &'a str> + 'a {
    let prefix = format!("{key}=");
    body.lines()
        .filter_map(move |l| l.trim().strip_prefix(prefix.as_str()).map(str::trim))
}

fn unit_row(unit: &UnitSpec, body: &str) -> Value {
    let profiles = if unit.profiles.is_empty() {
        ALL_PROFILES
    } else {
        unit.profiles
    };
    json!({
        "name": unit.stem(),
        "description": directive(body, "Description").next(),
        "category": unit.category.map(|c| c.as_str()),
        "profiles": profiles,
        "roles": unit.roles,
        "install": unit.install.as_str(),
        "start_on_install": unit.start_on_install,
        "api_restartable": unit.api_restartable,
        "drops_link": unit.drops_link,
        "memory_max": directive(body, "MemoryMax").next(),
        "condition_paths": directive(body, "ConditionPathExists").collect::<Vec<_>>(),
    })
}

fn main() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/systemd");
    let mut units: Vec<&UnitSpec> = UNITS.iter().collect();
    units.sort_by_key(|u| u.name);
    let rows: Vec<Value> = units
        .into_iter()
        .map(|u| {
            let path = dir.join(u.name);
            let body = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            unit_row(u, &body)
        })
        .collect();
    println!("{}", json!({ "schema": 1, "units": rows }));
}
