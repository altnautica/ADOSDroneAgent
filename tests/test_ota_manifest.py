"""Tests for OTA update manifest model."""

from __future__ import annotations

import pytest
from pydantic import ValidationError

from ados.services.ota.manifest import UpdateManifest


def _sample_data() -> dict:
    return {
        "version": "0.2.0",
        "channel": "stable",
        "published_at": "2026-03-08T00:00:00Z",
        "download_url": "https://updates.altnautica.com/stable/ados-0.2.0.bin",
        "file_size": 52428800,
        "sha256": "a" * 64,
        "changelog": "Bug fixes and performance improvements.",
        "release_url": "https://github.com/altnautica/ADOSDroneAgent/releases/tag/v0.2.0",
    }


def test_valid_manifest():
    m = UpdateManifest(**_sample_data())
    assert m.version == "0.2.0"
    assert m.channel == "stable"
    assert m.file_size == 52428800
    assert m.release_url.endswith("v0.2.0")


def test_manifest_all_fields_present():
    fields = set(UpdateManifest.model_fields.keys())
    expected = {
        "version", "channel", "published_at", "download_url",
        "file_size", "sha256", "changelog", "release_url",
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
