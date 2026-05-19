"""Tests for the rich install-dialog content fields on the plugin manifest.

These optional top-level fields are pure copy for the install dialog:
the agent does not enforce them, but the host parses them so the GCS
modal can render a richer pre-install summary. Older manifests without
these fields parse cleanly with sensible defaults.
"""

from __future__ import annotations

from pathlib import Path

import pytest
import yaml

from ados.plugins.errors import ManifestError
from ados.plugins.manifest import PluginManifest


def _yaml(d: dict) -> str:
    return yaml.safe_dump(d, sort_keys=False)


def _base_manifest_dict() -> dict:
    return {
        "schema_version": 2,
        "id": "com.example.rich",
        "version": "1.2.3",
        "name": "Example Rich",
        "license": "GPL-3.0-or-later",
        "risk": "medium",
        "compatibility": {"ados_version": ">=0.9.0,<1.0.0"},
        "agent": {
            "entrypoint": "agent/plugin.py",
            "permissions": [],
        },
    }


def test_rich_fields_absent_parse_cleanly() -> None:
    """Older manifests that omit every rich field still validate."""
    m = PluginManifest.from_yaml_text(_yaml(_base_manifest_dict()))
    assert m.description_long is None
    assert m.features == []
    assert m.hardware_requirements is None
    assert m.resource_impact is None
    assert m.required_fc_parameters is None
    assert m.telemetry_fields == []
    assert m.documentation_url is None
    assert m.screenshots == []


def test_rich_fields_round_trip() -> None:
    """A manifest carrying every rich field round-trips into the model."""
    d = _base_manifest_dict()
    d.update(
        {
            "description_long": "Long form description across\nmultiple lines.",
            "features": ["Feature A", "Feature B"],
            "hardware_requirements": {
                "cameras": "USB UVC",
                "fc_firmware": "ArduPilot 4.5+",
                "boards": ["cm4", "rk3582"],
                "optional": ["Rangefinder"],
            },
            "resource_impact": {
                "cpu_percent_peak": 80,
                "ram_mb": 512,
                "pids": 24,
                "startup_time_seconds": 5,
            },
            "required_fc_parameters": {
                "ardupilot": [
                    {"param": "EKF_SOURCE_SET", "note": "Pick vision lane"},
                ],
                "px4": [
                    {"param": "EKF2_AID_MASK", "note": "VISION bits"},
                ],
                "inav": [
                    {"param": "opflow_hardware", "value": "MAVLINK"},
                    {"param": "nav_use_optflow_for_poshold", "value": "ON"},
                ],
            },
            "telemetry_fields": [
                "navigation.estimator_state",
                "navigation.drift_m",
            ],
            "documentation_url": "https://docs.altnautica.com/some-page",
            "screenshots": [
                {"url": "https://example.com/a.png", "caption": "A"},
                {"url": "https://example.com/b.png"},
            ],
        }
    )
    m = PluginManifest.from_yaml_text(_yaml(d))

    assert m.description_long is not None
    assert m.description_long.startswith("Long form description")
    assert m.features == ["Feature A", "Feature B"]

    assert m.hardware_requirements is not None
    assert m.hardware_requirements.cameras == "USB UVC"
    assert m.hardware_requirements.fc_firmware == "ArduPilot 4.5+"
    assert m.hardware_requirements.boards == ["cm4", "rk3582"]
    assert m.hardware_requirements.optional == ["Rangefinder"]

    assert m.resource_impact is not None
    assert m.resource_impact.cpu_percent_peak == 80
    assert m.resource_impact.ram_mb == 512
    assert m.resource_impact.pids == 24
    assert m.resource_impact.startup_time_seconds == 5

    assert m.required_fc_parameters is not None
    ap = m.required_fc_parameters.ardupilot
    assert len(ap) == 1
    assert ap[0].param == "EKF_SOURCE_SET"
    assert ap[0].note == "Pick vision lane"
    inav = m.required_fc_parameters.inav
    assert inav[0].param == "opflow_hardware"
    assert inav[0].value == "MAVLINK"

    assert "navigation.estimator_state" in m.telemetry_fields
    assert m.documentation_url == "https://docs.altnautica.com/some-page"

    assert len(m.screenshots) == 2
    assert m.screenshots[0].url == "https://example.com/a.png"
    assert m.screenshots[0].caption == "A"
    assert m.screenshots[1].caption is None


def test_documentation_url_must_be_https() -> None:
    """http:// (or other schemes) rejected with a clear message."""
    d = _base_manifest_dict()
    d["documentation_url"] = "http://docs.altnautica.com/page"
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(_yaml(d))


def test_resource_impact_rejects_negative_ram() -> None:
    """RAM must be positive; zero or negative is a manifest error."""
    d = _base_manifest_dict()
    d["resource_impact"] = {"ram_mb": 0}
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(_yaml(d))


def test_resource_impact_allows_multi_core_cpu_peak() -> None:
    """Peak CPU above 100 is allowed for multi-core workloads."""
    d = _base_manifest_dict()
    d["resource_impact"] = {"cpu_percent_peak": 400}
    m = PluginManifest.from_yaml_text(_yaml(d))
    assert m.resource_impact is not None
    assert m.resource_impact.cpu_percent_peak == 400


def test_fc_parameter_param_is_required() -> None:
    """Each required-parameter entry must carry a ``param`` key."""
    d = _base_manifest_dict()
    d["required_fc_parameters"] = {
        "ardupilot": [{"note": "missing param key"}],
    }
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(_yaml(d))


def test_vision_nav_rich_field_subset_parses() -> None:
    """The rich-field subset of the shipped vision-nav manifest parses.

    The shipped ``ADOSExtensions/extensions/vision-nav/manifest.yaml``
    carries a few legacy top-level fields (``contains_vendor_binary``,
    ``vendor_attribution`` as a list, ``locales``) that the current
    strict Pydantic schema does not recognize; that drift predates this
    change. This test loads only the rich-content slice the install
    modal needs so we still exercise the vision-nav copy verbatim.
    """
    repo_root = Path(__file__).resolve().parents[2]
    manifest_path = (
        repo_root
        / "ADOSExtensions"
        / "extensions"
        / "vision-nav"
        / "manifest.yaml"
    )
    if not manifest_path.exists():
        pytest.skip(
            f"vision-nav manifest not present at {manifest_path}; "
            "run inside the monorepo to exercise this case"
        )

    raw = yaml.safe_load(manifest_path.read_text(encoding="utf-8"))
    rich_keys = {
        "description_long",
        "features",
        "hardware_requirements",
        "resource_impact",
        "required_fc_parameters",
        "telemetry_fields",
        "documentation_url",
        "screenshots",
    }
    slim = _base_manifest_dict()
    slim["id"] = "com.altnautica.vision-nav"
    slim["version"] = raw["version"]
    slim["name"] = raw["name"]
    for key in rich_keys:
        if key in raw:
            slim[key] = raw[key]

    m = PluginManifest.from_yaml_text(_yaml(slim))
    assert m.id == "com.altnautica.vision-nav"
    assert m.version == raw["version"]
    assert m.description_long is not None
    assert m.description_long.strip()
    assert m.features and len(m.features) >= 6
    assert m.hardware_requirements is not None
    assert "USB UVC" in (m.hardware_requirements.cameras or "")
    assert m.resource_impact is not None
    assert m.resource_impact.ram_mb == 512
    assert m.required_fc_parameters is not None
    assert any(
        p.param == "EKF_SOURCE_SET"
        for p in m.required_fc_parameters.ardupilot
    )
    assert "navigation.feature_count" in m.telemetry_fields
    assert m.documentation_url is not None
    assert m.documentation_url.startswith("https://")
