"""Capability catalog + grant-helper tests.

Covers ``ados.plugins.capabilities`` against supervisor state
(``get_granted_caps``, ``has_capability``, ``require_capability``) and pins the
generated catalog so a codegen change surfaces in review.

The per-method dispatch gate is NOT covered here any more. That gate lives in
the native host, and the tests that exercised it drove the packaged Python host
server, which no longer exists. The equivalent coverage is the host's own
capability-enforcement guard, which derives the gated set from the dispatch
table rather than restating it.
"""

from __future__ import annotations

import pytest

from ados.plugins.capabilities import (
    AGENT_CAPABILITIES,
    ENFORCED_AGENT_CAPABILITIES,
    get_granted_caps,
    has_capability,
    is_known_agent_capability,
    require_capability,
)
from ados.plugins.errors import CapabilityDenied
from ados.plugins.state import PermissionGrant, PluginInstall

PLUGIN_ID = "com.example.gated"


class _StubSupervisor:
    """Minimal supervisor stand-in for the helpers.

    The real supervisor exposes ``find_install``; the helpers only call
    that one method, so a lightweight stub keeps the tests fast and
    free of /var/ados side effects.
    """

    def __init__(self, installs: list[PluginInstall]) -> None:
        self._installs = installs

    def find_install(self, plugin_id: str) -> PluginInstall | None:
        for inst in self._installs:
            if inst.plugin_id == plugin_id:
                return inst
        return None


def _install_with_perms(plugin_id: str, **grants: bool) -> PluginInstall:
    return PluginInstall(
        plugin_id=plugin_id,
        version="1.0.0",
        source="local_file",
        source_uri=None,
        signer_id=None,
        manifest_hash="x" * 64,
        status="installed",
        installed_at=0,
        permissions={
            pid: PermissionGrant(granted=g, granted_at=0 if g else None)
            for pid, g in grants.items()
        },
    )


def test_catalog_size_matches_spec() -> None:
    # 52 agent capabilities in the catalog. Beyond the earlier substrate set
    # (button.subscribe, flight.guided_setpoint, mavlink.tunnel,
    # radio.aux_stream, display.oled.page) and the GPIO output + vision
    # designate/subscribe caps (hardware.gpio_out, vision.detection.subscribe,
    # vision.track.designate), the compute-offload caps (compute.dataset.write,
    # compute.job.submit, compute.job.read, compute.stream.open) back the plugin
    # compute-offload + streaming-perception methods, mcp.expose gates the
    # plugin's tool/resource/prompt exposure to the MCP surface, and
    # video.source.set lets a camera/pod driver reconfigure the video pipeline's
    # stream sources. msp.read/msp.write gate the MSP lane the way mavlink.read/
    # mavlink.write gate MAVLink, and vision.model.read lets a plugin read the
    # model catalog without being able to register into it.
    assert len(AGENT_CAPABILITIES) == 52


def test_gated_caps_are_marked_enforced() -> None:
    # Pins the set so a codegen change shows up in review. It is NOT the drift
    # guard: this list is hand-maintained and cannot see the Rust dispatch
    # table, so for a long time it happily asserted a set fifteen capabilities
    # smaller than what the code actually gated -- including mavlink.write,
    # which is the one that can arm motors. A hand-written list that claims to
    # describe generated truth will eventually describe the past.
    #
    # The real guard derives the set from the dispatch table plus the named
    # handler gates and fails if the catalog disagrees; it lives beside the code
    # it reads, in the plugin host's capability_enforcement_guard tests.
    assert ENFORCED_AGENT_CAPABILITIES == frozenset(
        {
            "compute.dataset.write",
            "compute.job.read",
            "compute.job.submit",
            "compute.stream.open",
            "button.subscribe",
            "display.oled.page",
            "estimator.pose.inject",
            "event.publish",
            "event.subscribe",
            "flight.guided_setpoint",
            "hardware.gpio_out",
            "mavlink.component.vio",
            "mavlink.read",
            "mavlink.tunnel",
            "mavlink.write",
            "mcp.expose",
            "mission.read",
            "mission.write",
            "msp.read",
            "msp.write",
            "process.spawn",
            "radio.aux_stream",
            "recording.write",
            "sensor.camera.register",
            "telemetry.extend",
            "telemetry.read",
            "video.source.set",
            "vision.detection.publish",
            "vision.detection.subscribe",
            "vision.frame.read",
            "vision.model.read",
            "vision.model.register",
            "vision.track.designate",
        }
    )


def test_is_known_agent_capability() -> None:
    assert is_known_agent_capability("event.publish")
    assert is_known_agent_capability("mavlink.read")
    assert not is_known_agent_capability("not.a.real.cap")


# ---------------------------------------------------------------------
# State-driven helpers
# ---------------------------------------------------------------------


def test_get_granted_caps_returns_only_granted() -> None:
    sup = _StubSupervisor(
        [
            _install_with_perms(
                PLUGIN_ID,
                **{"telemetry.read": True, "mission.write": False},
            )
        ]
    )
    assert get_granted_caps(sup, PLUGIN_ID) == {"telemetry.read"}


def test_get_granted_caps_unknown_plugin_returns_empty() -> None:
    sup = _StubSupervisor([])
    assert get_granted_caps(sup, "no.such.plugin") == set()


def test_has_capability_true_when_granted() -> None:
    sup = _StubSupervisor(
        [_install_with_perms(PLUGIN_ID, **{"mission.read": True})]
    )
    assert has_capability(sup, PLUGIN_ID, "mission.read")
    assert not has_capability(sup, PLUGIN_ID, "mission.write")


def test_require_capability_raises_on_missing() -> None:
    sup = _StubSupervisor([_install_with_perms(PLUGIN_ID)])
    with pytest.raises(CapabilityDenied) as excinfo:
        require_capability(sup, PLUGIN_ID, "vehicle.command")
    assert excinfo.value.plugin_id == PLUGIN_ID
    assert excinfo.value.capability == "vehicle.command"
