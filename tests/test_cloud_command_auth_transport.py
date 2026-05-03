"""Cloud command polling must not put API keys in URLs."""

from __future__ import annotations

from pathlib import Path


def test_cloud_relay_uses_header_auth_for_status_and_commands() -> None:
    service_sources = [
        Path("src/ados/services/cloud/__main__.py"),
        Path("src/ados/core/main.py"),
    ]

    for source_path in service_sources:
        source = source_path.read_text()
        assert '"X-ADOS-Key"' in source
        assert '"apiKey": pairing.api_key,\n                        "version":' not in source
        monolith_status_secret = (
            '"apiKey": self.pairing_manager.api_key,\n                        "version":'
        )
        assert monolith_status_secret not in source
        assert 'params={"deviceId": device_id, "apiKey": api_key}' not in source
        assert '"apiKey": api_key,\n                                        "status":' not in source
