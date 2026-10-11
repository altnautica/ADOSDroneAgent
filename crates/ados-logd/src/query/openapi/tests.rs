use super::*;

#[test]
fn openapi_document_lists_every_v1_path() {
    let doc = document();
    let paths = doc["paths"].as_object().unwrap();
    for p in [
        "/v1/query",
        "/v1/tail",
        "/v1/aggregate",
        "/v1/export",
        "/v1/sessions",
        "/v1/stats",
        "/v1/synced",
        "/v1/healthz",
        "/v1/openapi.json",
    ] {
        assert!(paths.contains_key(p), "missing path {p}");
    }
    assert_eq!(doc["openapi"], "3.0.3");
}

#[test]
fn synced_is_a_post_and_export_advertises_the_unsynced_filter() {
    let doc = document();
    // The mark endpoint is a POST with the request/response schemas.
    assert!(doc["paths"]["/v1/synced"]["post"].is_object());
    assert!(doc["components"]["schemas"]["SyncRequest"].is_object());
    assert!(doc["components"]["schemas"]["SyncResponse"].is_object());
    // The export operation advertises the unsynced filter the push uses.
    let params = doc["paths"]["/v1/export"]["get"]["parameters"]
        .as_array()
        .unwrap();
    let refs: Vec<&str> = params.iter().filter_map(|p| p["$ref"].as_str()).collect();
    assert!(refs.iter().any(|r| r.ends_with("/unsynced")));
}

#[test]
fn query_op_references_the_cursor_and_kind_params() {
    let doc = document();
    let params = doc["paths"]["/v1/query"]["get"]["parameters"]
        .as_array()
        .unwrap();
    let refs: Vec<&str> = params.iter().filter_map(|p| p["$ref"].as_str()).collect();
    assert!(refs.iter().any(|r| r.ends_with("/cursor")));
    assert!(refs.iter().any(|r| r.ends_with("/kind")));
}
