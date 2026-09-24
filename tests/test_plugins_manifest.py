"""Plugin manifest validation tests."""

from __future__ import annotations

import pytest
import yaml
from structlog.testing import capture_logs

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
                {"id": "hardware.i2c", "required": False},
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
    assert "hardware.i2c" in m.declared_permissions()
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


def test_target_profiles_accepts_workstation() -> None:
    raw = _good_manifest_dict()
    raw["agent"]["target_profiles"] = ["workstation"]
    m = PluginManifest.from_yaml_text(_yaml(raw))
    assert m.agent is not None
    assert m.agent.target_profiles == ["workstation"]


def test_target_profiles_accepts_multi_target() -> None:
    raw = _good_manifest_dict()
    raw["agent"]["target_profiles"] = ["drone", "ground-station", "workstation"]
    m = PluginManifest.from_yaml_text(_yaml(raw))
    assert m.agent is not None
    assert m.agent.target_profiles == ["drone", "ground-station", "workstation"]


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


def test_unknown_gcs_capability_warns_but_still_loads() -> None:
    raw = _good_manifest_dict()
    raw["gcs"]["permissions"] = ["ui.slot.fc-tab", "ui.slot.not-a-real-slot"]
    # Warn-only: the manifest still loads with the unknown permission, and
    # the unknown id is flagged in a warning rather than silently dropped.
    # Capture via structlog's testing sink so the assertion does not depend on
    # the global log config or the Python version's stdout/stderr routing.
    with capture_logs() as logs:
        m = PluginManifest.from_yaml_text(_yaml(raw))
    assert m.gcs is not None
    assert "ui.slot.not-a-real-slot" in {p.id for p in m.gcs.permissions}
    warnings = [
        e for e in logs if e.get("event") == "plugin_manifest_unknown_gcs_capability"
    ]
    assert warnings, "expected a plugin_manifest_unknown_gcs_capability warning"
    assert any(e.get("capability") == "ui.slot.not-a-real-slot" for e in warnings)


# ── profiles ─────────────────────────────────────────────────


def test_target_profiles_accepts_compute_and_normalises_ground_station() -> None:
    raw = _good_manifest_dict()
    raw["agent"]["target_profiles"] = ["compute", "ground_station", "ground-station"]
    m = PluginManifest.from_yaml_text(_yaml(raw))
    assert m.agent is not None
    assert m.agent.target_profiles == ["compute", "ground-station"]


# ── resource class ───────────────────────────────────────────


def test_heavy_class_allows_limits_above_standard_ceilings() -> None:
    raw = _good_manifest_dict()
    raw["agent"]["resources"] = {
        "class": "heavy",
        "max_ram_mb": 16384,
        "max_cpu_percent": 800,
        "max_pids": 1024,
    }
    m = PluginManifest.from_yaml_text(_yaml(raw))
    assert m.agent is not None
    assert m.agent.resources.resource_class == "heavy"
    assert m.agent.resources.max_ram_mb == 16384


@pytest.mark.parametrize(
    ("field", "value"),
    [("max_ram_mb", 4097), ("max_cpu_percent", 101), ("max_pids", 257)],
)
def test_standard_class_refuses_limits_above_its_ceiling(field: str, value: int) -> None:
    raw = _good_manifest_dict()
    raw["agent"]["resources"][field] = value
    with pytest.raises(ManifestError, match=field):
        PluginManifest.from_yaml_text(_yaml(raw))


def test_heavy_class_still_bounded_by_field_limits() -> None:
    raw = _good_manifest_dict()
    raw["agent"]["resources"] = {"class": "heavy", "max_ram_mb": 65537}
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(_yaml(raw))


# ── packaged binaries ────────────────────────────────────────


def _rust_bin_manifest() -> dict:
    raw = _good_manifest_dict()
    raw["agent"]["runtime"] = "rust"
    raw["agent"]["entrypoint"] = "bin:thermal-agent"
    raw["agent"]["binaries"] = {
        "thermal-agent": {
            "aarch64-linux": "bin/aarch64-linux/thermal-agent",
            "aarch64-macos": "bin/aarch64-macos/thermal-agent",
        }
    }
    return raw


def test_bin_entrypoint_with_matching_binary_parses() -> None:
    m = PluginManifest.from_yaml_text(_yaml(_rust_bin_manifest()))
    assert m.agent is not None
    assert m.agent.entrypoint == "bin:thermal-agent"


def test_bin_entrypoint_requires_a_binaries_key() -> None:
    raw = _rust_bin_manifest()
    raw["agent"]["binaries"] = {"other": {"aarch64-linux": "bin/other"}}
    with pytest.raises(ManifestError, match="binaries"):
        PluginManifest.from_yaml_text(_yaml(raw))


def test_bin_entrypoint_requires_rust_runtime() -> None:
    raw = _rust_bin_manifest()
    raw["agent"]["runtime"] = "python"
    with pytest.raises(ManifestError, match="runtime"):
        PluginManifest.from_yaml_text(_yaml(raw))


def test_service_bin_command_requires_a_binaries_key() -> None:
    raw = _rust_bin_manifest()
    raw["agent"]["contributes"] = {
        "services": [{"name": "indexer", "command": "bin:indexer --fast"}]
    }
    with pytest.raises(ManifestError, match="indexer"):
        PluginManifest.from_yaml_text(_yaml(raw))
    raw["agent"]["binaries"]["indexer"] = {"aarch64-linux": "bin/indexer"}
    PluginManifest.from_yaml_text(_yaml(raw))


@pytest.mark.parametrize(
    "binaries",
    [
        {"Thermal": {"aarch64-linux": "bin/t"}},
        {"thermal-agent": {"aarch64": "bin/t"}},
        {"thermal-agent": {"aarch64-linux": "../t"}},
        {"thermal-agent": {"aarch64-linux": "pkg:Class"}},
    ],
)
def test_malformed_binaries_rejected(binaries: dict) -> None:
    raw = _rust_bin_manifest()
    raw["agent"]["binaries"] = binaries
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(_yaml(raw))


# ── payloads ─────────────────────────────────────────────────


def _payload(**over: object) -> dict:
    payload = {
        "path": "models/net.onnx",
        "source": "https://example.com/net.onnx",
        "sha256": "a" * 64,
        "size_bytes": 1024,
    }
    payload.update(over)
    return payload


def test_payload_parses_and_normalises_profiles() -> None:
    raw = _good_manifest_dict()
    raw["agent"]["payloads"] = [
        _payload(profiles=["ground_station"], arch_os="x86_64-linux")
    ]
    m = PluginManifest.from_yaml_text(_yaml(raw))
    assert m.agent is not None
    assert m.agent.payloads[0].profiles == ["ground-station"]


@pytest.mark.parametrize(
    "over",
    [
        {"source": "http://example.com/net.onnx"},
        {"sha256": "A" * 64},
        {"sha256": "a" * 63},
        {"size_bytes": 0},
        {"size_bytes": 1_073_741_825},
        {"path": "../escape.bin"},
        {"arch_os": "linux"},
    ],
)
def test_payload_rules_refuse_bad_entries(over: dict) -> None:
    raw = _good_manifest_dict()
    raw["agent"]["payloads"] = [_payload(**over)]
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(_yaml(raw))


def test_payload_paths_must_be_unique() -> None:
    raw = _good_manifest_dict()
    raw["agent"]["payloads"] = [_payload(), _payload(sha256="b" * 64)]
    with pytest.raises(ManifestError, match="unique"):
        PluginManifest.from_yaml_text(_yaml(raw))


# ── declared capabilities and shared topics ──────────────────


def test_declared_capability_must_live_under_the_plugin_leaf() -> None:
    raw = _good_manifest_dict()
    raw["agent"]["declared_capabilities"] = [
        {"id": "plugin.thermal.frames.read", "risk": "low"}
    ]
    PluginManifest.from_yaml_text(_yaml(raw))
    raw["agent"]["declared_capabilities"] = [
        {"id": "plugin.other.frames.read", "risk": "low"}
    ]
    with pytest.raises(ManifestError, match="plugin.thermal."):
        PluginManifest.from_yaml_text(_yaml(raw))


def test_shared_topic_outside_plugin_namespace_refused() -> None:
    raw = _good_manifest_dict()
    raw["agent"]["contributes"] = {
        "shared_topics": [
            {"topic": "plugin.other.frames", "subscribe_capability": "event.subscribe"}
        ]
    }
    with pytest.raises(ManifestError, match="plugin.thermal."):
        PluginManifest.from_yaml_text(_yaml(raw))


def test_shared_topic_subscribe_capability_must_be_known_or_declared() -> None:
    raw = _good_manifest_dict()
    raw["agent"]["contributes"] = {
        "shared_topics": [
            {"topic": "plugin.thermal.frames", "subscribe_capability": "plugin.thermal.read"}
        ]
    }
    with pytest.raises(ManifestError, match="subscribe_capability"):
        PluginManifest.from_yaml_text(_yaml(raw))
    raw["agent"]["declared_capabilities"] = [
        {"id": "plugin.thermal.read", "risk": "medium"}
    ]
    m = PluginManifest.from_yaml_text(_yaml(raw))
    assert m.agent is not None
    assert m.agent.contributes.shared_topics[0].topic == "plugin.thermal.frames"


def test_plugin_declared_permission_does_not_warn() -> None:
    raw = _good_manifest_dict()
    raw["agent"]["permissions"] = ["plugin.lidar.scan.read"]
    with capture_logs() as logs:
        PluginManifest.from_yaml_text(_yaml(raw))
    assert not [
        e for e in logs if e.get("event") == "plugin_manifest_unknown_agent_capability"
    ]


# ── service listeners ────────────────────────────────────────


def test_listen_ports_require_listen_and_outbound_capabilities() -> None:
    raw = _good_manifest_dict()
    raw["agent"]["contributes"] = {
        "services": [{"name": "api", "command": "/bin/api", "listen_ports": [8080]}]
    }
    raw["agent"]["permissions"] = ["network.listen"]
    with pytest.raises(ManifestError, match="network.outbound"):
        PluginManifest.from_yaml_text(_yaml(raw))
    raw["agent"]["permissions"] = ["network.outbound"]
    with pytest.raises(ManifestError, match="network.listen"):
        PluginManifest.from_yaml_text(_yaml(raw))
    raw["agent"]["permissions"] = ["network.listen", "network.outbound"]
    PluginManifest.from_yaml_text(_yaml(raw))


@pytest.mark.parametrize("ports", [[80], [8080, 8080], [2000, 2001, 2002, 2003, 2004]])
def test_listen_ports_bounds(ports: list[int]) -> None:
    raw = _good_manifest_dict()
    raw["agent"]["permissions"] = ["network.listen", "network.outbound"]
    raw["agent"]["contributes"] = {
        "services": [{"name": "api", "command": "/bin/api", "listen_ports": ports}]
    }
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(_yaml(raw))


def test_service_profiles_normalised_and_empty_refused() -> None:
    raw = _good_manifest_dict()
    raw["agent"]["contributes"] = {
        "services": [{"name": "api", "command": "/bin/api", "profiles": ["ground_station"]}]
    }
    m = PluginManifest.from_yaml_text(_yaml(raw))
    assert m.agent is not None
    assert m.agent.contributes.services[0].profiles == ["ground-station"]
    raw["agent"]["contributes"]["services"][0]["profiles"] = []
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(_yaml(raw))


# ── gcs half ─────────────────────────────────────────────────


def test_gcs_worker_isolation_refused() -> None:
    raw = _good_manifest_dict()
    raw["gcs"]["isolation"] = "worker"
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(_yaml(raw))


def test_agent_pages_setup_for_rules() -> None:
    raw = _good_manifest_dict()
    raw["gcs"]["contributes"]["agent_pages"] = [
        {"id": "overview", "title": "Overview", "profile": ["compute"]},
        {"id": "setup", "title": "Setup", "setup_for": "overview"},
    ]
    PluginManifest.from_yaml_text(_yaml(raw))

    raw["gcs"]["contributes"]["agent_pages"][1]["setup_for"] = "missing"
    with pytest.raises(ManifestError, match="setup_for"):
        PluginManifest.from_yaml_text(_yaml(raw))

    raw["gcs"]["contributes"]["agent_pages"][1]["setup_for"] = "setup"
    with pytest.raises(ManifestError, match="own setup_for"):
        PluginManifest.from_yaml_text(_yaml(raw))

    raw["gcs"]["contributes"]["agent_pages"] = [
        {"id": "overview", "title": "Overview"},
        {"id": "setup", "title": "Setup", "setup_for": "overview"},
        {"id": "setup-extra", "title": "More", "setup_for": "setup"},
    ]
    with pytest.raises(ManifestError, match="itself a setup page"):
        PluginManifest.from_yaml_text(_yaml(raw))


def test_agent_page_ids_unique() -> None:
    raw = _good_manifest_dict()
    raw["gcs"]["contributes"]["agent_pages"] = [
        {"id": "overview", "title": "A"},
        {"id": "overview", "title": "B"},
    ]
    with pytest.raises(ManifestError, match="unique"):
        PluginManifest.from_yaml_text(_yaml(raw))


def test_node_surface_requires_profile() -> None:
    raw = _good_manifest_dict()
    raw["gcs"]["contributes"]["node_surfaces"] = [{"id": "temps", "title": "Temps"}]
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(_yaml(raw))
    raw["gcs"]["contributes"]["node_surfaces"][0]["profile"] = []
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(_yaml(raw))
    raw["gcs"]["contributes"]["node_surfaces"][0]["profile"] = ["ground_station"]
    raw["gcs"]["contributes"]["node_surfaces"][0]["group"] = "device"
    m = PluginManifest.from_yaml_text(_yaml(raw))
    assert m.gcs is not None
    assert m.gcs.contributes.node_surfaces[0].profile == ["ground-station"]
