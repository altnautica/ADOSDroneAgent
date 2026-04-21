"""Pydantic models for peripheral manifests.

A peripheral manifest is the declarative contract a plugin (or YAML
file) uses to tell the agent: "here is a device type I can handle,
here is how to spot it on a given transport, here is what I expose."
The registry merges entry-point manifests with filesystem manifests
and serves them over the REST API.

Schema is intentionally narrow so that external OEM partners have a
stable target before they ship any plugin code. Future revisions may
add vendor-specific sections under ``extra`` without breaking the
top-level shape.
"""

from __future__ import annotations

from pathlib import Path
from typing import Literal

import yaml
from pydantic import BaseModel, Field, ValidationError


class ManifestError(ValueError):
    """Raised when a peripheral manifest fails to load or validate."""


class PeripheralMatch(BaseModel):
    """Transport match spec. All fields optional; empty match matches nothing.

    For USB transports, ``vid`` and ``pid`` are hex strings like
    ``"0x1a2b"``. For serial or network transports, ``regex`` can match
    device paths or service names. At least one field should be set in
    practice, but we do not enforce that at schema level so plugins
    can ship "catch-all" manifests for diagnostic use.
    """

    vid: str | None = None
    pid: str | None = None
    regex: str | None = None


class PeripheralAction(BaseModel):
    """Action the peripheral exposes.

    The ``body_schema`` is an optional JSON Schema for validating the
    body of a POST to ``/api/v1/peripherals/{id}/action``. The current
    schema version does not enforce it; plugin-side validation lands
    with live transport detection.
    """

    id: str
    display_name: str
    requires_confirm: bool = False
    body_schema: dict | None = None


class PeripheralManifest(BaseModel):
    """Declarative manifest for a single peripheral type.

    The ``id`` is the unique identifier used in REST routes and
    registry lookups. Choose a short, dotted, lowercase string such
    as ``oem.rc.android`` or ``oem.controller.xyz``.
    """

    id: str
    display_name: str
    transport: Literal["usb", "serial", "network", "ble"]
    match: PeripheralMatch = Field(default_factory=PeripheralMatch)
    capabilities: list[str] = Field(default_factory=list)
    actions: list[PeripheralAction] = Field(default_factory=list)
    config_schema: dict | None = None
    status_endpoint: str | None = None
    extra: dict = Field(default_factory=dict)

    @classmethod
    def from_yaml_file(cls, path: str | Path) -> "PeripheralManifest":
        """Load and validate a manifest from a YAML file.

        Raises ManifestError with a path-qualified message on any
        failure (missing file, bad YAML, failed Pydantic validation).
        """
        resolved = Path(path)
        if not resolved.is_file():
            raise ManifestError(f"manifest file not found: {resolved}")

        try:
            with open(resolved, encoding="utf-8") as fh:
                raw = yaml.safe_load(fh)
        except (OSError, yaml.YAMLError) as exc:
            raise ManifestError(
                f"failed to read {resolved}: {exc}"
            ) from exc

        if not isinstance(raw, dict):
            raise ManifestError(
                f"manifest at {resolved} must be a YAML mapping, got {type(raw).__name__}"
            )

        try:
            return cls.model_validate(raw)
        except ValidationError as exc:
            raise ManifestError(
                f"manifest at {resolved} failed validation: {exc}"
            ) from exc
