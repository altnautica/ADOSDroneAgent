"""Tests for OTA update manifest model."""

from __future__ import annotations

import pytest
from pydantic import ValidationError

from ados.services.ota.manifest import UpdateManifest


def _sample_data() -> dict:
    return {
        "version": "0.2.0",
        "channel": "stable",
        "release_date": "2026-03-08T00:00:00Z",
        "download_url": "https://updates.altnautica.com/stable/ados-0.2.0.bin",
        "file_size": 52428800,
        "sha256": "a" * 64,
        "signature": "c2lnbmF0dXJl",
        "min_version": "0.1.0",
        "changelog": "Bug fixes and performance improvements.",
        "requires_reboot": True,
    }


def test_valid_manifest():
    m = UpdateManifest(**_sample_data())
    assert m.version == "0.2.0"
    assert m.channel == "stable"
    assert m.file_size == 52428800
    assert m.requires_reboot is True


def test_manifest_all_fields_present():
    m = UpdateManifest(**_sample_data())
    fields = set(m.model_fields.keys())
    expected = {
        "version", "channel", "release_date", "download_url",
        "file_size", "sha256", "signature", "min_version",
        "changelog", "requires_reboot",
    }
    assert fields == expected


def test_manifest_missing_required_field():
    data = _sample_data()
    del data["version"]
    with pytest.raises(ValidationError):
        UpdateManifest(**data)


def test_manifest_json_roundtrip():
    m = UpdateManifest(**_sample_data())
    json_str = m.model_dump_json()
    m2 = UpdateManifest.model_validate_json(json_str)
    assert m == m2
