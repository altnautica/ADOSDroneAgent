use super::*;

#[test]
fn a_registry_id_becomes_one_path_segment() {
    assert_eq!(
        registry_url("https://convex-site.example.com/", "com.example.thermal").as_deref(),
        Some("https://convex-site.example.com/v1/plugins/com.example.thermal")
    );
}

#[test]
fn an_id_that_could_leave_its_segment_is_refused() {
    for id in ["", "../admin", "a/b", "a?b=1", "a#b", "a b", "a%2F"] {
        assert_eq!(registry_url("https://c.example.com", id), None, "{id}");
    }
}
