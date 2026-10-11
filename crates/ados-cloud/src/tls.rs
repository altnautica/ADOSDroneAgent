//! Shared TLS client configuration.
//!
//! Builds a rustls [`ClientConfig`] backed by the ring crypto provider and the
//! bundled webpki trust anchors. The same config is handed to both the HTTPS
//! update poller (reqwest) and the MQTT-over-WSS transport (rumqttc), so there
//! is one TLS posture for the whole relay. The provider is the one swappable
//! seam: swapping it is a one-line change here.

use std::sync::Arc;

use rustls::ClientConfig;

/// A rustls client config that verifies servers against the bundled webpki
/// roots, using the ring provider. Suitable for both the HTTPS update poll and
/// the MQTT WSS transport.
pub fn client_config() -> ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
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
mod tests;
