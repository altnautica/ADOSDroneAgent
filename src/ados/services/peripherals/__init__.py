"""Peripheral Manager. Plugin registry for external devices.

The Peripheral Manager is a thin, profile-agnostic registry that lets
out-of-tree products (ADOS Edge per DEC-098, external OEM partners,
and future plugins) declare their peripherals as
manifests. The agent loads those manifests from two sources: Python
entry points under the group ``ados.peripherals`` and YAML files in
``/etc/ados/peripherals/*.yaml``. Each manifest describes how to match
a device on a transport (USB, serial, network, BLE), what capabilities
it exposes, which actions can be invoked on it, and an optional
JSON Schema for its runtime config.

Wave 3 ships the schema, loader, registry, systemd service, and REST
surface under ``/api/v1/peripherals/*``. Live transport detection and
plugin-driven action handling land in Track B once real plugins exist.

Exports:
    PeripheralManifest: Pydantic model for a manifest.
    PeripheralMatch: Transport-level match spec.
    PeripheralAction: Action declaration.
    PeripheralRegistry: Runtime registry class.
    get_peripheral_registry: Process-wide singleton accessor.
"""

from __future__ import annotations

from ados.services.peripherals.manifest import (
    PeripheralAction,
    PeripheralManifest,
    PeripheralMatch,
)
from ados.services.peripherals.registry import (
    PeripheralRegistry,
    get_peripheral_registry,
)

__all__ = [
    "PeripheralAction",
    "PeripheralManifest",
    "PeripheralMatch",
    "PeripheralRegistry",
    "get_peripheral_registry",
]
