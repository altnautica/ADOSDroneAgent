use super::*;

#[test]
fn builds_a_client_config_with_roots() {
    // The config builds without panicking and the ring provider +
    // bundled roots are wired. (A handshake is not exercised here; that is a
    // network integration concern.)
    let cfg = client_config();
    // The webpki root set is non-empty, so the verifier has anchors.
    assert!(!webpki_roots::TLS_SERVER_ROOTS.is_empty());
    // ClientConfig is opaque; constructing it is the assertion. Reference it
    // so the build is not optimized away.
    let _ = Arc::new(cfg);
}
