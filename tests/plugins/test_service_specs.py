"""Plugin-declared supervised services: manifest coercion and validation.

Covers the additive ``agent.contributes.services`` shape: the legacy
``list[str]`` form still parses, the rich ``list[ServiceSpec]`` form
parses, and an unsafe ready check is refused at parse time. Unit rendering,
lifecycle and readiness live in the native plugin host.
"""

from __future__ import annotations

import pytest

from ados.plugins.manifest import AgentContributes, PluginManifest, ServiceSpec

# ---------------------------------------------------------------------
# Manifest coercion + parsing
# ---------------------------------------------------------------------


def test_legacy_string_services_still_parse() -> None:
    """Old ``services: ["foo"]`` coerces each bare string to a spec."""
    c = AgentContributes.model_validate({"services": ["foo", "bar"]})
    assert [s.name for s in c.services] == ["foo", "bar"]
    # The bare string becomes both the name and the exec command.
    assert c.services[0].command == "foo"
    assert c.services[0].ready_check is None
    assert c.services[0].restart == "on-failure"
    assert c.services[0].slice == "ados-plugins.slice"


def test_rich_service_specs_parse() -> None:
    c = AgentContributes.model_validate(
        {
            "services": [
                {
                    "name": "sensor-daemon",
                    "command": "/opt/ados/plugins/com.example.x/bin/run",
                    "ready_check": "http://127.0.0.1:9100/healthz",
                    "restart": "always",
                },
            ]
        }
    )
    assert len(c.services) == 1
    spec = c.services[0]
    assert spec.name == "sensor-daemon"
    assert spec.command.endswith("/bin/run")
    assert spec.ready_check == "http://127.0.0.1:9100/healthz"
    assert spec.restart == "always"


def test_mixed_legacy_and_rich_services_parse() -> None:
    c = AgentContributes.model_validate(
        {"services": ["legacy", {"name": "rich", "command": "echo hi"}]}
    )
    assert [s.name for s in c.services] == ["legacy", "rich"]
    assert c.services[0].command == "legacy"
    assert c.services[1].command == "echo hi"


def test_empty_services_default() -> None:
    c = AgentContributes.model_validate({})
    assert c.services == []
    assert c.service_specs() == []


def test_service_name_rejects_uppercase() -> None:
    from ados.plugins.errors import ManifestError

    with pytest.raises(ManifestError):
        ServiceSpec.model_validate({"name": "BadName", "command": "x"})


def test_service_command_required() -> None:
    # A rich entry must carry a non-empty command.
    with pytest.raises(Exception):
        ServiceSpec.model_validate({"name": "ok"})


def test_full_manifest_with_services_parses() -> None:
    manifest = PluginManifest.from_yaml_text(
        """\
schema_version: 1
id: com.example.daemonplug
version: 0.1.0
name: Daemon Plug
license: GPL-3.0-or-later
risk: medium
compatibility:
  ados_version: ">=0.0.0"
agent:
  entrypoint: agent/plugin.py
  isolation: subprocess
  permissions: ["event.publish"]
  contributes:
    services:
      - name: worker
        command: /opt/ados/plugins/com.example.daemonplug/bin/worker
        ready_check: "cmd:/bin/true"
        restart: always
"""
    )
    specs = manifest.agent.contributes.services
    assert len(specs) == 1
    assert specs[0].name == "worker"
    assert specs[0].ready_check == "cmd:/bin/true"


@pytest.mark.parametrize(
    "ready_check",
    [
        "/bin/true\nExecStartPre=+/bin/sh",
        "/bin/true\x00",
        "http://192.168.1.50:9100/healthz",
        "http://127.0.0.1/healthz",
        "https://user:pw@127.0.0.1:9100/",
        "   ",
        "'unterminated",
    ],
)
def test_manifest_rejects_an_unsafe_ready_check(ready_check: str) -> None:
    from ados.plugins.errors import ManifestError

    with pytest.raises(ManifestError):
        ServiceSpec.model_validate(
            {"name": "worker", "command": "noop", "ready_check": ready_check}
        )
