"""Parity tests for the generated WFB adapter tables.

The WFB-ng compatible chipset table, the driver-name fallback, and the two
management-WiFi deny-sets are generated from
``crates/ados-protocol/wfb-adapters.toml`` into
:mod:`ados.services.wfb._wfb_tables_generated` (the Python copy) and
``crates/ados-protocol/src/wfb_tables.rs`` (the Rust copy). The codegen's
``--check`` drift gate keeps the two generated files aligned with the TOML.

These tests pin the Python side: the four generated tables match the
canonical contract (a drift in the generated module or an accidental
hand-edit is a test failure). The Rust side locks the same values in its own
parity tests; the codegen ``--check`` ties both generated files to the one
TOML.
"""

from __future__ import annotations

from ados.services.wfb._wfb_tables_generated import (
    WFB_COMPATIBLE,
    WFB_COMPATIBLE_DRIVERS,
    WFB_DENY_DRIVER_PREFIXES,
    WFB_DENY_USB_VENDORS,
)

# The canonical tables, locked here so a drift in the generated module is a
# test failure. The Rust side locks the same values byte-for-byte.
EXPECTED_COMPATIBLE: dict[tuple[int, int], str] = {
    (0x0BDA, 0x8812): "RTL8812AU",
    (0x0BDA, 0x881A): "RTL8812AU (alt)",
    (0x0BDA, 0x881B): "RTL8812AU (alt)",
    (0x0BDA, 0x881C): "RTL8812AU (alt)",
    (0x0BDA, 0xA81A): "RTL8812AU (a81a)",
    (0x0BDA, 0xB812): "RTL8812EU",
    (0x2357, 0x0120): "RTL8812AU (TP-Link)",
    (0x2357, 0x0101): "RTL8812AU (TP-Link alt)",
}
EXPECTED_DRIVERS: frozenset[str] = frozenset(
    {"8812au", "8812eu", "rtl8812au", "rtl8812eu", "rtl88x2eu", "rtl88xxau"}
)
EXPECTED_DENY_VENDORS: frozenset[int] = frozenset({0xA69C})
EXPECTED_DENY_PREFIXES: tuple[str, ...] = ("aic8800", "brcmfmac")


def test_generated_compatible_table_matches_contract() -> None:
    assert WFB_COMPATIBLE == EXPECTED_COMPATIBLE


def test_generated_driver_fallback_matches_contract() -> None:
    assert WFB_COMPATIBLE_DRIVERS == EXPECTED_DRIVERS


def test_generated_deny_sets_match_contract() -> None:
    assert WFB_DENY_USB_VENDORS == EXPECTED_DENY_VENDORS
    assert WFB_DENY_DRIVER_PREFIXES == EXPECTED_DENY_PREFIXES
