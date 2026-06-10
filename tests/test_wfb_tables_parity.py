"""Parity tests for the generated WFB adapter tables.

The WFB-ng compatible chipset table, the driver-name fallback, and the two
management-WiFi deny-sets are generated from
``crates/ados-protocol/wfb-adapters.toml`` into
:mod:`ados.services.wfb._wfb_tables_generated` (the Python copy) and
``crates/ados-protocol/src/wfb_tables.rs`` (the Rust copy). The codegen's
``--check`` drift gate keeps the two generated files aligned with the TOML,
so the Rust radio service and the Python iw-fallback / mesh adapters cannot
drift on which adapter is WFB-capable or which management radio is denied.

These tests pin the Python side:

1. The four generated tables match the canonical contract (a drift in the
   generated module or an accidental hand-edit is a test failure). The Rust
   side locks the same values in its own parity tests; the codegen
   ``--check`` ties both generated files to the one TOML.
2. ``adapter.py`` re-exports the generated names, so its importers (the
   FastAPI read route, the mesh relay/receiver) keep importing them
   unchanged.
3. The deny-set is wired into the management-WiFi gate.
"""

from __future__ import annotations

from ados.services.wfb import adapter as adapter_mod
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


def test_adapter_module_reexports_generated_tables() -> None:
    # The importers (FastAPI route, mesh relay/receiver) read these names from
    # adapter.py; the lift must keep them identical to the generated module.
    assert adapter_mod.WFB_COMPATIBLE is WFB_COMPATIBLE
    assert adapter_mod.WFB_COMPATIBLE_DRIVERS is WFB_COMPATIBLE_DRIVERS
    assert adapter_mod.WFB_DENY_USB_VENDORS is WFB_DENY_USB_VENDORS
    assert adapter_mod.WFB_DENY_DRIVER_PREFIXES is WFB_DENY_DRIVER_PREFIXES


def test_management_wifi_deny_gate_uses_the_generated_sets() -> None:
    # AIC8800 (Rock 5C onboard) and brcmfmac (Pi onboard) must always be denied
    # so the manager never auto-picks a non-injection management radio.
    assert adapter_mod._is_denied_management_wifi(0xA69C, "some_driver")
    assert adapter_mod._is_denied_management_wifi(0, "aic8800_fdrv")
    assert adapter_mod._is_denied_management_wifi(0, "brcmfmac")
    # A real RTL injection adapter is never denied.
    assert not adapter_mod._is_denied_management_wifi(0x0BDA, "rtl88x2eu")
