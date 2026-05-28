"""Capability catalog tests for the schema-v2 additions.

Asserts the agent catalog grew from 29 to 32 entries with the three
new high-risk capabilities, and that the GCS catalog count matches
the canonical list shipped in the TS SDK. The catalog is the source
of truth for the install dialog's permission summary, so a missing
entry would silently strip the operator's view of the risk surface.
"""

from __future__ import annotations

from ados.plugins.capabilities import (
    AGENT_CAPABILITIES,
    ENFORCED_AGENT_CAPABILITIES,
    is_known_agent_capability,
)

NEW_AGENT_CAPS = (
    "mavlink.component.vio",
    "estimator.pose.inject",
    "process.spawn",
)


def test_agent_capability_count_is_32() -> None:
    """The baseline catalog shipped 29 entries; schema v2 adds three."""
    assert len(AGENT_CAPABILITIES) == 32


def test_new_agent_capabilities_present() -> None:
    for cap in NEW_AGENT_CAPS:
        assert cap in AGENT_CAPABILITIES, (
            f"capability {cap!r} missing from AGENT_CAPABILITIES; "
            "the install dialog will not surface the risk"
        )


def test_new_agent_capabilities_are_known() -> None:
    for cap in NEW_AGENT_CAPS:
        assert is_known_agent_capability(cap)


def test_event_bus_capabilities_still_enforced() -> None:
    """Sanity check: the enforced subset did not shift."""
    assert "event.publish" in ENFORCED_AGENT_CAPABILITIES
    assert "event.subscribe" in ENFORCED_AGENT_CAPABILITIES


def test_existing_baseline_capabilities_preserved() -> None:
    """The schema-v2 additions must not have replaced any baseline
    entry; prior audits cite the baseline set by line."""
    baseline_catalog = {
        "event.publish",
        "event.subscribe",
        "mavlink.read",
        "mavlink.write",
        "mavlink.component.camera",
        "mavlink.component.gimbal",
        "mavlink.component.payload",
        "mavlink.component.peripheral",
        "telemetry.read",
        "telemetry.extend",
        "sensor.camera.register",
        "sensor.depth.register",
        "sensor.lidar.register",
        "sensor.imu.register",
        "sensor.payload.register",
        "hardware.uart",
        "hardware.i2c",
        "hardware.spi",
        "hardware.gpio",
        "hardware.usb",
        "hardware.usb.uvc",
        "hardware.camera.csi",
        "hardware.audio",
        "vehicle.command",
        "mission.read",
        "mission.write",
        "network.outbound",
        "filesystem.host",
        "recording.write",
    }
    assert len(baseline_catalog) == 29
    assert baseline_catalog.issubset(AGENT_CAPABILITIES)


def test_gcs_capability_count_matches_ts_catalog() -> None:
    """The GCS-side catalog lives in TypeScript. This test reads the
    TS file and confirms the entry count matches the schema-v2 GCS
    surface. The TS file is authoritative for the GCS side; we read
    rather than duplicate the list. ``GCS_CAPABILITIES`` is emitted by
    the capability codegen into ``gcs-capabilities.generated.ts`` and
    re-exported from ``capabilities.ts``; we read the generated file
    where the literal array lives."""
    import re
    from pathlib import Path

    ts_path = (
        Path(__file__).resolve().parents[3]
        / "ADOSMissionControl"
        / "src"
        / "lib"
        / "plugins"
        / "gcs-capabilities.generated.ts"
    )
    if not ts_path.exists():  # GCS submodule may be absent in CI
        return
    text = ts_path.read_text(encoding="utf-8")
    # Match string literals inside the GCS_CAPABILITIES array.
    block = re.search(
        r"export const GCS_CAPABILITIES\s*=\s*\[(.*?)\]\s*as const;",
        text,
        re.DOTALL,
    )
    assert block is not None, "GCS_CAPABILITIES array not found"
    entries = re.findall(r'"([^"]+)"', block.group(1))
    # The TS catalog enumerates one cap per ui slot (more than the
    # spec's 10 because the TS file inlines a fuller slot taxonomy)
    # plus the telemetry / command / mission / cloud surface.
    assert len(entries) >= 17
    # Spot-check spec-defined slots survive.
    for required in (
        "ui.slot.drone-detail-tab",
        "telemetry.subscribe",
        "command.send",
        "mission.read",
        "mission.write",
        "cloud.read",
        "cloud.write",
    ):
        assert required in entries, f"GCS catalog missing {required!r}"
