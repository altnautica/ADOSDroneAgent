//! Keeps `schemas/api/heartbeat.schema.json` in step with
//! [`HeartbeatPayload`]. The committed schema is what
//! `ados-capabilities-codegen` turns into the TypeScript heartbeat types
//! Mission Control and its Convex backend read. Regenerate with
//! `ADOS_BLESS_SCHEMAS=1 cargo test -p ados-cloud heartbeat_schema`.

use super::HeartbeatPayload;
use std::path::PathBuf;

/// The schema as the payload serializes: optional-and-skipped fields are not
/// required, a nullable field without a skip is required and nullable.
fn render() -> String {
    let schema = schemars::generate::SchemaSettings::draft2020_12()
        .for_serialize()
        .into_generator()
        .into_root_schema_for::<HeartbeatPayload>();
    serde_json::to_string_pretty(&schema).expect("schema serializes") + "\n"
}

#[test]
fn heartbeat_schema_is_current() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/api/heartbeat.schema.json");
    let expected = render();
    if std::fs::read_to_string(&path).unwrap_or_default() == expected {
        return;
    }
    if std::env::var("ADOS_BLESS_SCHEMAS").as_deref() == Ok("1") {
        std::fs::create_dir_all(path.parent().expect("schema dir")).expect("create schema dir");
        std::fs::write(&path, expected).expect("write schema");
        return;
    }
    panic!(
        "{} is out of date with HeartbeatPayload; run \
         ADOS_BLESS_SCHEMAS=1 cargo test -p ados-cloud heartbeat_schema",
        path.display()
    );
}
