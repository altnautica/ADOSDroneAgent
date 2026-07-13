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
    CAPABILITY_CATALOG,
    ENFORCED_AGENT_CAPABILITIES,
    is_known_agent_capability,
)

NEW_AGENT_CAPS = (
    "mavlink.component.vio",
    "estimator.pose.inject",
    "process.spawn",
)

# The current size of the generated agent capability catalog. The catalog is the
# single source of truth (generated from capabilities.toml); this test guards
# that the count does not change unnoticed, while the internal-consistency check
# below proves the catalog and its metadata table agree.
EXPECTED_AGENT_CAPABILITY_COUNT = 47

# The compute + vision capability family: gated at the dispatch level today, so
# the catalog must mark them enforced (the metadata that drives the install
# dialog's "has a runtime gate" signal must be honest — Rule 44).
COMPUTE_VISION_FAMILY = (
    "compute.job.submit",
    "compute.job.read",
    "compute.dataset.write",
    "compute.stream.open",
    "vision.frame.read",
    "vision.model.register",
    "vision.detection.publish",
    "vision.detection.subscribe",
    "vision.track.designate",
)


def test_agent_capability_count() -> None:
    """The agent catalog is the source of truth (generated from
    ``capabilities.toml``). Assert its size and that the catalog and its
    metadata table are internally consistent (no orphans, no duplicates), so a
    drift between the two is caught here rather than at import time."""
    # No duplicates: a frozenset can't hold them, but assert the count matches
    # the de-duplicated set explicitly so an accidental TOML duplicate (which the
    # codegen would collapse) is visible as a count drop.
    assert len(AGENT_CAPABILITIES) == len(set(AGENT_CAPABILITIES))
    assert len(AGENT_CAPABILITIES) == EXPECTED_AGENT_CAPABILITY_COUNT
    # The catalog and its metadata table describe the same set of capabilities.
    assert set(AGENT_CAPABILITIES) == set(CAPABILITY_CATALOG.keys())


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


def test_compute_stream_open_capability_exists() -> None:
    """The streaming perception-offload open capability is in the catalog + its
    metadata table (the install dialog surfaces its risk)."""
    assert "compute.stream.open" in AGENT_CAPABILITIES
    assert is_known_agent_capability("compute.stream.open")
    assert "compute.stream.open" in CAPABILITY_CATALOG


def test_compute_and_vision_family_is_enforced() -> None:
    """The compute + vision family carries a runtime dispatch gate today, so the
    catalog must mark each enforced — a lie here would train an operator to
    distrust the install dialog's risk signal (Rule 44)."""
    for cap in COMPUTE_VISION_FAMILY:
        assert cap in AGENT_CAPABILITIES, f"{cap!r} missing from the catalog"
        assert cap in ENFORCED_AGENT_CAPABILITIES, (
            f"{cap!r} is gated at dispatch but not marked enforced in the catalog"
        )


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
        "ui.slot.node-detail-tab",
        "telemetry.subscribe",
        "command.send",
        "mission.read",
        "mission.write",
        "cloud.read",
        "cloud.write",
    ):
        assert required in entries, f"GCS catalog missing {required!r}"
