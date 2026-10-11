//! Dump the native route table as JSON, for `scripts/gen-api-surface.py`.
//!
//! The generator needs the exact `(method, path)` set the auth edge serves. It
//! could parse the registration sources, but a regex over source is a second,
//! weaker definition of the surface. This calls the real route builder instead
//! (through `routing::native_route_table`), so the committed
//! `docs/api-surface.md` cannot describe a surface the agent does not have.
//!
//! ```text
//! cargo run --quiet -p ados-control --example api-surface
//! ```

fn main() {
    let rows: Vec<String> = ados_control::routing::native_route_table()
        .into_iter()
        .map(|(method, path)| format!(r#"{{"method":"{method}","path":"{path}"}}"#))
        .collect();
    println!("[{}]", rows.join(","));
}
