//! The committed route table must describe the surface this build serves.
//!
//! `docs/api-surface.md` is what five clients are checked against
//! (`scripts/check-api-surface.py`), so a stale table is worse than none: it
//! would clear a client calling a route that no longer exists, and reject one
//! calling a route that now does. This fails the moment the native half drifts
//! from `routing::native_route_table()`, naming the regenerate command.
//!
//! Only the native half is asserted here. The residual half comes from
//! importing the FastAPI routers, which this process cannot do; the Python
//! side is covered by the generator being the only writer and by the client
//! check failing on anything it omits.

use std::collections::BTreeSet;

use ados_control::routing::native_route_table;

const TABLE: &str = include_str!("../../../docs/api-surface.md");

const REGENERATE: &str =
    "regenerate with: ADOSDroneAgent/.venv/bin/python scripts/gen-api-surface.py";

/// Every `(METHOD, path)` row in one `##` section of the committed table.
///
/// Section-aware, because a path alone does not identify which half serves
/// it: `GET /api/video/config` is native and `POST /api/video/config` is
/// residual. Keying on the path collapses those two into one, which is the
/// same `(method, path)`-vs-`path` confusion that had the generator erasing
/// the residual row outright.
fn rows_in_section(heading_prefix: &str) -> BTreeSet<(String, String)> {
    let mut rows = BTreeSet::new();
    let mut inside = false;
    for line in TABLE.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            inside = rest.starts_with(heading_prefix);
            continue;
        }
        if !inside {
            continue;
        }
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        if cells.len() < 4 {
            continue;
        }
        let method = cells[1];
        let path = cells[2].trim_matches('`');
        if path.starts_with('/') && method.chars().all(|c| c.is_ascii_uppercase()) {
            rows.insert((method.to_string(), path.to_string()));
        }
    }
    rows
}

/// Every `(METHOD, path)` row in the committed table, from every section.
fn committed_rows() -> BTreeSet<(String, String)> {
    let mut rows = rows_in_section("Native");
    rows.extend(rows_in_section("Residual"));
    rows.extend(rows_in_section("Logging store"));
    rows
}

fn native_rows() -> BTreeSet<(String, String)> {
    native_route_table()
        .into_iter()
        .map(|(m, p)| (m.to_string(), p.to_string()))
        .collect()
}

#[test]
fn every_native_route_appears_in_the_committed_table() {
    let committed = committed_rows();
    let missing: Vec<_> = native_rows().difference(&committed).cloned().collect();
    assert!(
        missing.is_empty(),
        "these routes are served but absent from docs/api-surface.md, so the \
         client check would reject a caller that is actually correct: {missing:?}\n{REGENERATE}"
    );
}

#[test]
fn the_table_claims_no_native_route_this_build_does_not_serve() {
    // The dangerous direction: a row for a route that was deleted clears a
    // client still calling it, which is the silent 404 the table exists to
    // prevent.
    //
    // Compared against the NATIVE SECTION, not against every row whose path
    // happens to match a native one. The old filter did the latter, so a
    // residual row sharing a path with a native route under a different
    // method read as a stale native row — a false positive whose obvious
    // "fix" is deleting a correct row.
    let native = native_rows();
    let stale: Vec<_> = rows_in_section("Native")
        .into_iter()
        .filter(|row| !native.contains(row))
        .collect();
    assert!(
        stale.is_empty(),
        "docs/api-surface.md carries these native rows that this build does not \
         serve: {stale:?}\n{REGENERATE}"
    );
}

#[test]
fn the_table_covers_the_whole_native_surface_not_a_sample() {
    // A truncated or half-written table would pass the two assertions above
    // vacuously if the parser silently matched nothing.
    let committed = committed_rows();
    assert_eq!(
        native_rows().len(),
        native_route_table().len(),
        "native_route_table has duplicate (method, path) rows"
    );
    assert!(
        committed.len() > native_rows().len(),
        "the table ({} rows) should carry the native surface ({}) plus the \
         residual one; it looks truncated\n{REGENERATE}",
        committed.len(),
        native_rows().len()
    );
}
