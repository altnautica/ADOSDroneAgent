"""Regression tests for secure MQTT TLS defaults."""

from __future__ import annotations

import ssl
from pathlib import Path


def test_cloud_relays_do_not_disable_tls_verification() -> None:
    relay_sources = [
        Path("src/ados/services/cloud/mavlink_relay.py"),
        Path("src/ados/services/cloud/webrtc_signaling.py"),
        Path("src/ados/services/mqtt/gateway.py"),
    ]

    for source_path in relay_sources:
        source = source_path.read_text()
        assert "tls_insecure_set(True)" not in source
        assert "ssl.CERT_NONE" not in source


def test_cloud_relays_require_certificate_verification() -> None:
    relay_sources = [
        Path("src/ados/services/cloud/mavlink_relay.py"),
        Path("src/ados/services/cloud/webrtc_signaling.py"),
        Path("src/ados/services/mqtt/gateway.py"),
    ]

    for source_path in relay_sources:
        source = source_path.read_text()
        assert "cert_reqs=ssl.CERT_REQUIRED" in source

    assert ssl.CERT_REQUIRED != ssl.CERT_NONE
