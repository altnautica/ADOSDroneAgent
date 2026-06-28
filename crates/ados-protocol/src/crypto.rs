//! Process-wide rustls crypto provider install.
//!
//! The workspace's reqwest is unified onto the no-provider rustls path
//! (ados-cloud's posture: RustCrypto, no ring / aws-lc, so the agent stays a
//! clean musl static build). A reqwest `Client::builder().build()` that does not
//! supply a preconfigured TLS config constructs a DEFAULT config, which needs a
//! process-default crypto provider — without one it panics "No provider set",
//! non-deterministically under concurrent first builds. Every bare reqwest
//! client builder in the agent calls [`ensure_crypto_provider`] first so the
//! install is deterministic + race-free.

use std::sync::Once;

/// Install the RustCrypto rustls provider as the process default, exactly once.
/// An `Err` from `install_default` means a provider is already installed (e.g.
/// a preconfigured-TLS client beat us to it), which is fine.
pub fn ensure_crypto_provider() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::CryptoProvider::install_default(rustls_rustcrypto::provider());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_is_idempotent_and_does_not_panic() {
        // Calling it (possibly after another crate already installed a provider)
        // must never panic, and a second call is a no-op.
        ensure_crypto_provider();
        ensure_crypto_provider();
    }
}
