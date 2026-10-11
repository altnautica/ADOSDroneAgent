//! Router-wide guards: every table row reaches a handler, every path uses the
//! axum 0.8 parameter syntax, and every route module has a caller.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use tower::util::ServiceExt;

use super::*;

/// The registration sources, read at test time.
fn register_sources() -> Vec<(PathBuf, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/routes/register");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("register/ is readable") {
        let path = entry.expect("readable dir entry").path();
        if path.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&path).expect("readable source");
            out.push((path, text));
        }
    }
    assert!(!out.is_empty(), "no registration sources found");
    out
}

/// A concrete request path for a template: each `{name}` becomes one segment,
/// a `{*name}` tail becomes two.
fn concrete(template: &str) -> String {
    template
        .split('/')
        .map(|seg| {
            if seg.starts_with("{*") {
                "tail/end"
            } else if seg.starts_with('{') {
                "x1"
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Every row of the native table is a request the router serves. The fallback
/// answers a sentinel and a route layer answers a marker before any handler
/// runs, so the check exercises routing alone: a row the router never
/// registered (or registered under another method or path spelling) reaches the
/// fallback or the method router's 405, never the marker.
#[tokio::test]
async fn sentinel_every_table_row_reaches_a_handler() {
    const REACHED: StatusCode = StatusCode::NO_CONTENT;
    const SENTINEL: StatusCode = StatusCode::IM_A_TEAPOT;
    let (router, table) = build(true);
    let (_dir, state) = recording_route_tests::state_for_profile("drone");
    let app = router
        .route_layer(middleware::from_fn(|_req: Request, _next: Next| async {
            REACHED.into_response()
        }))
        .fallback(|| async { SENTINEL })
        .with_state(state);
    assert!(!table.is_empty());
    for row in &table {
        let uri = concrete(row.path);
        let request = Request::builder()
            .method(row.method.clone())
            .uri(&uri)
            .body(Body::empty())
            .unwrap();
        let status = app.clone().oneshot(request).await.unwrap().status();
        assert_eq!(
            status, REACHED,
            "{} {} (as {uri}) did not reach a handler",
            row.method, row.path
        );
    }
}

/// The HID writes are registered, and so native, only when hid-rust is on.
#[test]
fn the_hid_rows_follow_the_hid_flag() {
    let claim = |table: &[table::NativeRoute]| {
        table
            .iter()
            .any(|r| r.path == "/api/v1/ground-station/pic/claim")
    };
    assert!(claim(&describe(true)));
    assert!(!claim(&describe(false)));
}

/// axum 0.8 path parameters use the `{name}` / `{*name}` form. The 0.7
/// `:name` / `*name` form is rejected by axum 0.8 when the router is built, and
/// the edge's matcher reads only the brace form.
#[test]
fn route_paths_use_axum_08_param_syntax() {
    let offenders: Vec<String> = register_sources()
        .iter()
        .flat_map(|(_, src)| {
            src.lines()
                .map(str::trim_start)
                .filter(|l| {
                    (l.starts_with("\"/") || l.contains("(\"/"))
                        && (l.contains("/:") || l.contains("/*"))
                })
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "route path(s) use the :param / *param (axum 0.7) syntax; axum 0.8 needs {{param}}: {offenders:?}"
    );
}

/// Every module under `routes/` must have a caller somewhere in the crate.
///
/// A `pub mod` in a lib crate suppresses the dead-code warning, so an uncalled
/// module compiles clean and looks reachable to anyone reading the directory.
/// Deliberately "has a caller in the crate", NOT "is registered on the router":
/// several modules are reached by a sibling route or a background reconciler.
/// Text-matched over the crate's sources, with `//` lines dropped so an
/// intra-doc cross-reference is not counted as a call.
#[test]
fn every_route_module_has_a_caller() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let routes_dir = root.join("routes");
    let declared: Vec<String> = include_str!("mod.rs")
        .lines()
        .map(str::trim_start)
        .filter_map(|l| l.strip_prefix("pub mod "))
        .filter_map(|l| l.strip_suffix(';'))
        .map(str::to_string)
        .collect();
    assert!(
        !declared.is_empty(),
        "no route modules found; the parse broke"
    );

    let mut sources: Vec<(PathBuf, String)> = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("crate sources are readable") {
            let path = entry.expect("readable dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    let code: String = text
                        .lines()
                        .filter(|l| !l.trim_start().starts_with("//"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    sources.push((path, code));
                }
            }
        }
    }

    // A module's own file (or directory) is not its caller. Keyed on the path,
    // not the file stem: `register/logs.rs` calls `routes::logs`.
    let is_own = |path: &Path, m: &str| {
        path == routes_dir.join(format!("{m}.rs")) || path.starts_with(routes_dir.join(m))
    };
    let called = |m: &str| {
        let needle = format!("{m}::");
        sources
            .iter()
            .any(|(path, text)| !is_own(path, m) && text.contains(&needle))
    };

    // Modules deliberately built and deliberately not called yet. Naming them
    // is the difference between "inert on purpose" and "forgotten".
    //
    // `reachable_addr` detects a node advertising an address it cannot be
    // reached on. Held report-only on purpose: nothing consumes its verdict.
    const DELIBERATELY_UNCALLED: [&str; 1] = ["reachable_addr"];

    let unreached: Vec<&String> = declared
        .iter()
        .filter(|m| !DELIBERATELY_UNCALLED.contains(&m.as_str()))
        .filter(|m| !called(m))
        .collect();
    assert!(
        unreached.is_empty(),
        "module(s) declared under routes/ with no caller anywhere in the crate; \
         wire them up, delete them, or record them in DELIBERATELY_UNCALLED \
         with the reason: {unreached:?}"
    );

    let now_called: Vec<&str> = DELIBERATELY_UNCALLED
        .iter()
        .copied()
        .filter(|m| called(m))
        .collect();
    assert!(
        now_called.is_empty(),
        "module(s) listed as deliberately uncalled now have callers; \
         remove them from DELIBERATELY_UNCALLED: {now_called:?}"
    );
}
