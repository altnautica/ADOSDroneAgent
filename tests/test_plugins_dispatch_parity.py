"""Parity tests for the plugin RPC dispatch gate.

The ``method -> required_cap`` mapping is generated from the
``[[method]]`` section of ``crates/ados-protocol/capabilities.toml`` into
:mod:`ados.plugins._dispatch_generated` (the Python copy) and
``crates/ados-protocol/src/dispatch.rs`` (the Rust copy). The codegen's
``--check`` drift gate keeps the two generated files byte-aligned with the
TOML, so the Rust and Python hosts cannot disagree on a gate.

These tests pin the Python side of that contract:

1. ``build_dispatch_table`` sources every method's required cap from the
   generated :data:`REQUIRED_CAP` table, never an inline literal.
2. A handler registered for a method missing from the generated table is a
   loud import-time failure, not a silent ungated route.
3. The four vision methods carry a non-None gate in the generated table.
   This is the exact drift the codegen closes: before the lift, the Python
   table omitted them, so a future vision handler would have shipped
   ungated.
"""

from __future__ import annotations

import pytest

from ados.plugins._dispatch_generated import (
    INLINE_GATED,
    KNOWN_METHODS,
    REQUIRED_CAP,
)
from ados.plugins.ipc.dispatch import build_dispatch_table
from ados.plugins.ipc_server import PluginIpcServer

# The canonical method -> required-cap mapping. Locked here so a drift in the
# generated table (or an accidental hand-edit) is a test failure. The Rust
# enum locks the same set on its side (``enum_matches_generated_table``), and
# the codegen ``--check`` gate guarantees both generated files come from the
# one TOML, so this list IS the cross-language contract.
EXPECTED: dict[str, str | None] = {
    "event.publish": None,
    "event.subscribe": None,
    "ping": None,
    "tool.invoke": "mcp.expose",
    "telemetry.subscribe": "telemetry.read",
    "telemetry.extend": "telemetry.extend",
    "mission.read": "mission.read",
    "mission.write": "mission.write",
    "recording.start": "recording.write",
    "recording.stop": "recording.write",
    "mavlink.subscribe": "mavlink.read",
    "mavlink.send": "mavlink.write",
    "mavlink.tunnel.send": "mavlink.tunnel",
    "mavlink.register_component": None,
    "peripheral.register_driver": None,
    "peripheral.unregister_driver": None,
    "camera.claim": "sensor.camera.register",
    "camera.release": "sensor.camera.register",
    "camera.get_frame": "sensor.camera.register",
    "config.get": None,
    "config.set": None,
    "process.spawn": "process.spawn",
    "vision.subscribe_frames": "vision.frame.read",
    "vision.register_model": "vision.model.register",
    "vision.infer": "vision.model.register",
    "vision.publish_detection": "vision.detection.publish",
    "vision.subscribe_detections": "vision.detection.subscribe",
    "vision.designate_track": "vision.track.designate",
    "display.page.set": "display.oled.page",
    "gpio.output.set": "hardware.gpio_out",
    "gpio.buzzer.beep": "hardware.gpio_out",
    "flight.guided_setpoint.send": "flight.guided_setpoint",
    "radio.aux_stream.open": "radio.aux_stream",
    "radio.aux_stream.close": "radio.aux_stream",
    "compute.job.submit": "compute.job.submit",
    "compute.job.read": "compute.job.read",
    "compute.job.cancel": "compute.job.submit",
    "compute.job.outputs": "compute.job.read",
    "compute.dataset.write": "compute.dataset.write",
    "compute.stream.open": "compute.stream.open",
    "compute.stream.close": "compute.stream.open",
    "compute.stream.health": "compute.stream.open",
}


def test_generated_required_cap_matches_the_canonical_table() -> None:
    assert REQUIRED_CAP == EXPECTED
    assert KNOWN_METHODS == frozenset(EXPECTED)


def test_inline_gated_set_is_the_payload_gated_methods() -> None:
    # The three methods whose cap is decided inline from the request payload
    # have a None dispatch-level cap but are NOT open. The set must name
    # exactly them, so a reader can tell an open method from an inline one.
    assert INLINE_GATED == frozenset(
        {
            "mavlink.register_component",
            "peripheral.register_driver",
            "peripheral.unregister_driver",
        }
    )
    for method in INLINE_GATED:
        assert REQUIRED_CAP[method] is None


def test_build_dispatch_table_sources_cap_from_generated_table() -> None:
    table = build_dispatch_table(PluginIpcServer)
    # Every method the Python host can route takes its required cap from the
    # generated table, never a literal.
    for method, (_handler, requires) in table.items():
        assert method in REQUIRED_CAP, f"{method} not in the generated table"
        assert requires == REQUIRED_CAP[method], (
            f"{method} cap {requires!r} disagrees with the generated "
            f"table {REQUIRED_CAP[method]!r}"
        )


def test_dispatch_table_refuses_a_handler_missing_from_generated_table() -> None:
    # A handler registered for a method the generated table does not know would
    # run with no gate. The builder refuses to construct the table.
    class _Server(PluginIpcServer):
        pass

    import ados.plugins.ipc.dispatch as dispatch_mod

    original = dispatch_mod.REQUIRED_CAP
    try:
        # Drop a known method so its handler row has no generated cap.
        dispatch_mod.REQUIRED_CAP = {
            k: v for k, v in original.items() if k != "ping"
        }
        with pytest.raises(RuntimeError, match="not in the generated"):
            build_dispatch_table(_Server)
    finally:
        dispatch_mod.REQUIRED_CAP = original


def test_vision_methods_are_gated_in_the_generated_table() -> None:
    # The exact gap this lift closes: every vision method must carry a
    # non-None dispatch-level cap so it can never reach a host ungated.
    vision = {
        "vision.subscribe_frames": "vision.frame.read",
        "vision.register_model": "vision.model.register",
        "vision.infer": "vision.model.register",
        "vision.publish_detection": "vision.detection.publish",
    }
    for method, cap in vision.items():
        assert REQUIRED_CAP.get(method) == cap, f"{method} must gate on {cap}"
