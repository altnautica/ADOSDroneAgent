"""Plugin manifest v2 round-trip tests.

Verifies the three new agent-block fields parse and re-emit cleanly,
that the cross-field validators reject the bad-combo cases, and that
v1 manifests still load without churn for backward compatibility.
"""

from __future__ import annotations

import pytest
import yaml

from ados.plugins.errors import ManifestError
from ados.plugins.manifest import PluginManifest


def _yaml(d: dict) -> str:
    return yaml.safe_dump(d, sort_keys=False)


def _v2_manifest_dict() -> dict:
    """A v2 manifest exercising every new field."""
    return {
        "schema_version": 2,
        "id": "com.example.vision-nav",
        "version": "1.0.0",
        "name": "Vision Navigation",
        "license": "GPL-3.0-or-later",
        "risk": "critical",
        "compatibility": {"ados_version": ">=0.9.0,<1.0.0"},
        "agent": {
            "entrypoint": "agent/plugin.py",
            "isolation": "subprocess",
            "permissions": [
                "hardware.usb.uvc",
                "mavlink.component.vio",
                "estimator.pose.inject",
                "process.spawn",
                "event.publish",
            ],
            "mavlink_components": [
                {"component_id": 197, "component_kind": "vio"},
            ],
            "contains_vendor_binary": True,
            "vendor_attribution": [
                {
                    "upstream_repo": "https://github.com/rpng/open_vins",
                    "commit_sha": "6f1d2a0badc0ffee",
                    "license": "GPL-3.0-or-later",
                    "source_offer_url": (
                        "https://altnautica.com/oss/source-offer/openvins-6f1d2a.tar.gz"
                    ),
                },
            ],
            "subprocess_spawn": [
                "bin/openvins_run",
                "bin/openvins_calibrate",
            ],
            "per_drone_config": True,
        },
    }


def _v1_baseline_dict() -> dict:
    """A v1 manifest (no v2 fields). Must parse unchanged under v2 code."""
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
            "permissions": ["hardware.spi"],
        },
    }


def test_v2_manifest_parses() -> None:
    m = PluginManifest.from_yaml_text(_yaml(_v2_manifest_dict()))
    assert m.schema_version == 2
    assert m.agent is not None
    assert m.agent.contains_vendor_binary is True
    assert len(m.agent.vendor_attribution) == 1
    assert m.agent.vendor_attribution[0].commit_sha.startswith("6f1d2a")
    assert m.agent.vendor_attribution[0].license == "GPL-3.0-or-later"
    assert m.agent.subprocess_spawn == [
        "bin/openvins_run",
        "bin/openvins_calibrate",
    ]
    assert m.agent.per_drone_config is True
    assert m.agent.mavlink_components[0].component_kind == "vio"


def test_v1_manifest_still_parses_under_v2_code() -> None:
    """Backward compatibility: a manifest with no v2 fields must not
    regress under the v2 validator changes."""
    m = PluginManifest.from_yaml_text(_yaml(_v1_baseline_dict()))
    assert m.schema_version == 1
    assert m.agent is not None
    assert m.agent.vendor_attribution == []
    assert m.agent.subprocess_spawn is None
    assert m.agent.per_drone_config is False


def test_vendor_binary_without_attribution_rejected() -> None:
    bad = _v2_manifest_dict()
    bad["agent"].pop("vendor_attribution")
    with pytest.raises(ManifestError, match="vendor_attribution"):
        PluginManifest.from_yaml_text(_yaml(bad))


def test_attribution_without_vendor_binary_rejected() -> None:
    bad = _v2_manifest_dict()
    bad["agent"]["contains_vendor_binary"] = False
    with pytest.raises(ManifestError, match="contains_vendor_binary"):
        PluginManifest.from_yaml_text(_yaml(bad))


def test_subprocess_spawn_without_capability_rejected() -> None:
    bad = _v2_manifest_dict()
    bad["agent"]["permissions"] = [
        p for p in bad["agent"]["permissions"] if p != "process.spawn"
    ]
    with pytest.raises(ManifestError, match="process.spawn"):
        PluginManifest.from_yaml_text(_yaml(bad))


def test_subprocess_spawn_empty_allowed_without_capability() -> None:
    """Empty / absent allowlist must not force the capability."""
    ok = _v2_manifest_dict()
    ok["agent"]["subprocess_spawn"] = []
    ok["agent"]["permissions"] = [
        p for p in ok["agent"]["permissions"] if p != "process.spawn"
    ]
    m = PluginManifest.from_yaml_text(_yaml(ok))
    assert m.agent is not None
    assert m.agent.subprocess_spawn == []


def test_per_drone_config_defaults_false() -> None:
    """Default must stay false so existing fleet-wide plugins do not
    silently flip into per-drone scope on upgrade."""
    m = PluginManifest.from_yaml_text(_yaml(_v1_baseline_dict()))
    assert m.agent is not None
    assert m.agent.per_drone_config is False


def test_vio_component_kind_accepted() -> None:
    """``vio`` is a v2 addition to MavlinkComponent.component_kind."""
    only_vio = _v1_baseline_dict()
    only_vio["agent"]["mavlink_components"] = [
        {"component_id": 197, "component_kind": "vio"},
    ]
    m = PluginManifest.from_yaml_text(_yaml(only_vio))
    assert m.agent is not None
    assert m.agent.mavlink_components[0].component_kind == "vio"


def test_schema_version_3_rejected() -> None:
    """Schema version must stay bounded; v3 is not defined yet."""
    bad = _v2_manifest_dict()
    bad["schema_version"] = 3
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(_yaml(bad))


def test_round_trip_preserves_v2_fields() -> None:
    """Parse, re-emit via model_dump, parse again, fields stable."""
    src = _v2_manifest_dict()
    m1 = PluginManifest.from_yaml_text(_yaml(src))
    dumped = m1.model_dump(mode="python")
    m2 = PluginManifest.model_validate(dumped)
    assert m2.agent is not None
    assert m2.agent.subprocess_spawn == src["agent"]["subprocess_spawn"]
    assert m2.agent.per_drone_config is True
    assert len(m2.agent.vendor_attribution) == 1
    assert (
        m2.agent.vendor_attribution[0].upstream_repo
        == src["agent"]["vendor_attribution"][0]["upstream_repo"]
    )


def test_schema_dict_emits_new_definitions() -> None:
    from ados.plugins.manifest import schema_dict

    schema = schema_dict()
    assert "VendorAttribution" in schema["$defs"]
    agent_props = schema["$defs"]["AgentBlock"]["properties"]
    assert "vendor_attribution" in agent_props
    assert "subprocess_spawn" in agent_props
    assert "per_drone_config" in agent_props
