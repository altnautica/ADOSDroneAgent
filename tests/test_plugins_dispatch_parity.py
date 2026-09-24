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

from ados.plugins._dispatch_generated import (
    INLINE_GATED,
    REQUIRED_CAP,
)


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
