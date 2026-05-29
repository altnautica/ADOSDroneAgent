//! Shared TLS client configuration.
//!
//! Builds a rustls [`ClientConfig`] backed by the RustCrypto crypto provider
//! and the bundled webpki trust anchors. The same config is handed to both the
//! HTTPS update poller (reqwest) and the MQTT-over-WSS transport (rumqttc), so
//! there is one TLS posture for the whole relay. Using the RustCrypto provider
//! keeps the build free of any C crypto toolchain (no ring, no aws-lc, no
//! OpenSSL), which is what lets the crate cross-compile to
//! `aarch64-unknown-linux-musl` as a clean static binary. The provider is the
//! one swappable seam: swapping it is a one-line change here.

use std::sync::Arc;

use rustls::ClientConfig;

/// A rustls client config that verifies servers against the bundled webpki
/// roots, using the RustCrypto provider. Suitable for both the HTTPS update
/// poll and the MQTT WSS transport.
pub fn client_config() -> ClientConfig {
    let provider = Arc::new(rustls_rustcrypto::provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("rustls accepts the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth()
}

/// The same config wrapped in an `Arc`, the shape rumqttc's
/// `TlsConfiguration::Rustls` wants.
pub fn client_config_arc() -> Arc<ClientConfig> {
    Arc::new(client_config())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_client_config_with_roots() {
        // The config builds without panicking and the RustCrypto provider +
        // bundled roots are wired. (A handshake is not exercised here; that is a
        // network integration concern.)
        let cfg = client_config();
        // The webpki root set is non-empty, so the verifier has anchors.
        assert!(!webpki_roots::TLS_SERVER_ROOTS.is_empty());
        // ClientConfig is opaque; constructing it is the assertion. Reference it
        // so the build is not optimized away.
        let _ = Arc::new(cfg);
    }
}
