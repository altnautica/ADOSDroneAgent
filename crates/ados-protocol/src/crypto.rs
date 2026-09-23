//! Process-wide rustls crypto provider install.
//!
//! The workspace's reqwest is unified onto the no-provider rustls path
//! (ados-cloud hands every client it builds a preconfigured config). A reqwest
//! `Client::builder().build()` that does not supply a preconfigured TLS config
//! constructs a DEFAULT config, which needs a process-default crypto provider —
//! without one it panics "No provider set", non-deterministically under
//! concurrent first builds. Every bare reqwest client builder in the agent calls
//! [`ensure_crypto_provider`] first so the install is deterministic + race-free.

use std::sync::Once;

/// Install the ring rustls provider as the process default, exactly once.
/// An `Err` from `install_default` means a provider is already installed (e.g.
/// a preconfigured-TLS client beat us to it), which is fine.
pub fn ensure_crypto_provider() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_is_idempotent_and_installs_a_default() {
        // Calling it (possibly after another crate already installed a provider)
        // must never panic, a second call is a no-op, and afterwards a default
        // provider is present for a bare client builder to find.
        ensure_crypto_provider();
        ensure_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }
}
