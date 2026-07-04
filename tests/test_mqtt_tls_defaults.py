"""Regression tests for secure MQTT/TLS defaults across the cloud relay.

The broker transport moved to the native ``ados-cloud`` crate (rumqttc
over WSS + rustls); the surviving Python path (the in-process MQTT
gateway) still builds a paho client with certificate verification. It
must refuse to disable TLS verification.
"""

from __future__ import annotations

import ssl
from pathlib import Path

# Python paths that still construct a paho MQTT client with a TLS context.
_PY_TLS_SOURCES = (Path("src/ados/services/mqtt/gateway.py"),)

# The native broker transport and its shared rustls config.
_RUST_TLS_CONFIG = Path("crates/ados-cloud/src/tls.rs")
_RUST_TRANSPORT = Path("crates/ados-cloud/src/mqtt/transport.rs")


def test_python_relays_do_not_disable_tls_verification() -> None:
    for source_path in _PY_TLS_SOURCES:
        source = source_path.read_text()
        assert "tls_insecure_set(True)" not in source
        assert "ssl.CERT_NONE" not in source


def test_python_relays_require_certificate_verification() -> None:
    for source_path in _PY_TLS_SOURCES:
        source = source_path.read_text()
        assert "cert_reqs=ssl.CERT_REQUIRED" in source
    assert ssl.CERT_REQUIRED != ssl.CERT_NONE


def test_native_relay_verifies_server_certificates() -> None:
    """The native rustls config trusts the bundled webpki roots and never
    installs a verifier that accepts invalid certificates."""
    tls = _RUST_TLS_CONFIG.read_text()
    assert "webpki_roots::TLS_SERVER_ROOTS" in tls
    for danger in ("dangerous(", "danger_accept_invalid", "ServerCertVerifier"):
        assert danger not in tls, danger
    # The broker transport hands that verified config to the WSS connection.
    transport = _RUST_TRANSPORT.read_text()
    assert "client_config_arc()" in transport
    assert "Wss" in transport
