"""Plugin manifest validation tests."""

from __future__ import annotations

import pytest
import yaml

from ados.plugins.errors import ManifestError
from ados.plugins.manifest import PluginManifest


def _yaml(d: dict) -> str:
    return yaml.safe_dump(d, sort_keys=False)


def _good_manifest_dict() -> dict:
    return {
        "schema_version": 1,
        "id": "com.example.thermal",
        "version": "0.1.0",
        "name": "Example Thermal",
        "license": "GPL-3.0-or-later",
        "risk": "medium",
        "compatibility": {"ados_version": ">=0.9.0,<1.0.0"},
        "agent": {
            "entrypoint": "agent/plugin.py",
            "isolation": "subprocess",
            "permissions": [
                "hardware.spi",
                {"id": "vehicle.command", "required": False},
            ],
            "resources": {
                "max_ram_mb": 64,
                "max_cpu_percent": 25,
                "max_pids": 8,
            },
            "mavlink_components": [
                {"component_id": 100, "component_kind": "camera"}
            ],
        },
        "gcs": {
            "entrypoint": "gcs/plugin.bundle.js",
            "isolation": "iframe",
            "permissions": ["ui.slot.fc-tab"],
            "contributes": {
                "panels": [{"slot": "fc-tab", "id": "thermal"}],
                "notifications": [
                    {"id": "thermal-alarm", "title": "Thermal alarm", "severity": "warn"}
                ],
            },
        },
    }


def test_good_manifest_parses() -> None:
    m = PluginManifest.from_yaml_text(_yaml(_good_manifest_dict()))
    assert m.id == "com.example.thermal"
    assert m.agent is not None
    assert m.gcs is not None
    assert "hardware.spi" in m.declared_permissions()
    assert "vehicle.command" in m.declared_permissions()
    assert "ui.slot.fc-tab" in m.declared_permissions()
    assert m.agent.permissions[0].required is True  # bare-string permission
    assert m.agent.permissions[1].required is False  # object form


def test_id_must_be_reverse_dns() -> None:
    bad = _good_manifest_dict()
    bad["id"] = "thermal"
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(_yaml(bad))


def test_id_uppercase_rejected() -> None:
    bad = _good_manifest_dict()
    bad["id"] = "com.Example.thermal"
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(_yaml(bad))


def test_version_must_be_semver() -> None:
    bad = _good_manifest_dict()
    bad["version"] = "1"
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(_yaml(bad))


def test_at_least_one_half_required() -> None:
    bad = _good_manifest_dict()
    del bad["agent"]
    del bad["gcs"]
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(_yaml(bad))


def test_agent_only_is_allowed() -> None:
    only = _good_manifest_dict()
    del only["gcs"]
    m = PluginManifest.from_yaml_text(_yaml(only))
    assert m.agent is not None
    assert m.gcs is None


def test_gcs_only_is_allowed() -> None:
    only = _good_manifest_dict()
    del only["agent"]
    m = PluginManifest.from_yaml_text(_yaml(only))
    assert m.gcs is not None
    assert m.agent is None


def test_extra_top_level_keys_rejected() -> None:
    bad = _good_manifest_dict()
    bad["surprise"] = 1
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(_yaml(bad))


def test_resource_limits_clamped() -> None:
    bad = _good_manifest_dict()
    bad["agent"]["resources"]["max_ram_mb"] = 1
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(_yaml(bad))
    bad2 = _good_manifest_dict()
    bad2["agent"]["resources"]["max_cpu_percent"] = 200
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(_yaml(bad2))


def test_unknown_isolation_rejected() -> None:
    bad = _good_manifest_dict()
    bad["agent"]["isolation"] = "container"
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(_yaml(bad))


def test_invalid_yaml_rejected() -> None:
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text("not: valid: yaml: at all: [")


def test_top_level_must_be_mapping() -> None:
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text("- a\n- b\n")


def test_schema_dict_is_emittable() -> None:
    from ados.plugins.manifest import schema_dict

    schema = schema_dict()
    assert schema["type"] == "object"
    assert "properties" in schema


# ── target_profiles ──────────────────────────────────────────


def test_target_profiles_defaults_to_drone_when_absent() -> None:
    raw = _good_manifest_dict()
    raw["agent"].pop("target_profiles", None)
    m = PluginManifest.from_yaml_text(_yaml(raw))
    assert m.agent is not None
    assert m.agent.target_profiles == ["drone"]


def test_target_profiles_accepts_ground_station() -> None:
    raw = _good_manifest_dict()
    raw["agent"]["target_profiles"] = ["ground-station"]
    m = PluginManifest.from_yaml_text(_yaml(raw))
    assert m.agent is not None
    assert m.agent.target_profiles == ["ground-station"]


def test_target_profiles_accepts_multi_target() -> None:
    raw = _good_manifest_dict()
    raw["agent"]["target_profiles"] = ["drone", "ground-station"]
    m = PluginManifest.from_yaml_text(_yaml(raw))
    assert m.agent is not None
    assert m.agent.target_profiles == ["drone", "ground-station"]


def test_target_profiles_dedupes_repeated_entries() -> None:
    raw = _good_manifest_dict()
    raw["agent"]["target_profiles"] = ["drone", "drone", "ground-station"]
    m = PluginManifest.from_yaml_text(_yaml(raw))
    assert m.agent is not None
    assert m.agent.target_profiles == ["drone", "ground-station"]


def test_target_profiles_rejects_empty_list() -> None:
    raw = _good_manifest_dict()
    raw["agent"]["target_profiles"] = []
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(_yaml(raw))


def test_target_profiles_rejects_unknown_profile() -> None:
    raw = _good_manifest_dict()
    raw["agent"]["target_profiles"] = ["spacecraft"]
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(_yaml(raw))


# ── gcs unknown-capability warning ───────────────────────────


def test_known_gcs_capability_does_not_warn(capsys) -> None:
    raw = _good_manifest_dict()
    raw["gcs"]["permissions"] = ["ui.slot.fc-tab"]
    m = PluginManifest.from_yaml_text(_yaml(raw))
    assert m.gcs is not None
    captured = capsys.readouterr()
    assert "plugin_manifest_unknown_gcs_capability" not in (
        captured.out + captured.err
    )


def test_unknown_gcs_capability_warns_but_still_loads(capsys) -> None:
    raw = _good_manifest_dict()
    raw["gcs"]["permissions"] = ["ui.slot.fc-tab", "ui.slot.not-a-real-slot"]
    m = PluginManifest.from_yaml_text(_yaml(raw))
    # Warn-only: the manifest still loads with the unknown permission, and
    # the unknown id is flagged in a warning rather than silently dropped.
    assert m.gcs is not None
    assert "ui.slot.not-a-real-slot" in {p.id for p in m.gcs.permissions}
    captured = capsys.readouterr()
    blob = captured.out + captured.err
    assert "plugin_manifest_unknown_gcs_capability" in blob
    assert "ui.slot.not-a-real-slot" in blob
