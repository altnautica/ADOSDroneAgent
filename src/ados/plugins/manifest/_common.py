"""Shared patterns, helpers and base model for the plugin manifest."""

from __future__ import annotations

import re
from typing import Any, Literal, get_args

from pydantic import BaseModel, ConfigDict

from ados.core.logging import get_logger
from ados.plugins.errors import ManifestError

log = get_logger("plugins.manifest")

PLUGIN_ID_PATTERN = re.compile(r"^[a-z0-9]+(\.[a-z0-9-]+)+$")
"""Reverse-DNS plugin ids: at least two dotted segments, lowercase plus digits
plus hyphen-only-after-first-char inside segments."""

SEMVER_PATTERN = re.compile(
    r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
    r"(?:-((?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)(?:\.(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*))*))?"
    r"(?:\+([0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*))?$"
)

Profile = Literal["drone", "ground-station", "workstation", "compute", "visor"]

PROFILES: tuple[str, ...] = get_args(Profile)
"""Node profiles a plugin can target. Mirrors ``ados_config::node_profile()``."""

_PROFILE_ALIASES = {"ground_station": "ground-station"}
"""Underscore spellings accepted on input and rewritten to the canonical id."""

BIN_NAME_PATTERN = re.compile(r"^[a-z0-9][a-z0-9_-]{0,63}$")
"""Name of a packaged binary, referenced as ``bin:<name>``."""

ARCH_OS_PATTERN = re.compile(r"^[a-z0-9_]+-[a-z0-9_]+$")
"""``<arch>-<os>`` build key, e.g. ``aarch64-linux`` or ``aarch64-macos``."""

DECLARED_CAP_NAME_PATTERN = re.compile(r"^[a-z0-9][a-z0-9._-]{0,63}$")
"""Name segment of a plugin-declared capability ``plugin.<leaf>.<name>``."""

SHARED_TOPIC_PATTERN = re.compile(r"^plugin\.[a-z0-9-]+\.[a-z0-9._-]+$")
"""A topic a plugin publishes for other plugins: ``plugin.<leaf>.<rest>``."""

CONTRIBUTION_ID_PATTERN = re.compile(r"^[a-z0-9][a-z0-9_-]{0,63}$")
"""Id of a GCS agent page or node surface contribution."""

SHA256_HEX_PATTERN = re.compile(r"^[0-9a-f]{64}$")

BIN_PREFIX = "bin:"

STANDARD_RESOURCE_CEILINGS: dict[str, int] = {
    "max_ram_mb": 4096,
    "max_cpu_percent": 100,
    "max_pids": 256,
}
"""Per-field ceilings for the ``standard`` resource class. The ``heavy``
class is bounded only by the field limits on :class:`ResourceLimits`."""


def _plugin_leaf(plugin_id: str) -> str:
    """Last dot-segment of a plugin id (``com.example.world-engine`` ->
    ``world-engine``); the namespace for its declared capabilities and topics."""
    return plugin_id.rsplit(".", 1)[-1]


def _normalize_profiles(raw: Any) -> Any:
    """Rewrite accepted profile spellings to the canonical id before the
    ``Literal`` check runs."""
    if not isinstance(raw, list):
        return raw
    return [_PROFILE_ALIASES.get(p, p) if isinstance(p, str) else p for p in raw]


def _require_non_empty_profiles(value: list[str] | None, field: str) -> None:
    if value is not None and not value:
        raise ManifestError(f"{field} must list at least one profile when present")


def _bin_reference(token: str, field: str) -> str | None:
    """The ``<name>`` of a ``bin:<name>`` token, or ``None`` when ``token`` is
    not a packaged-binary reference. Refuses a malformed name."""
    if not token.startswith(BIN_PREFIX):
        return None
    name = token[len(BIN_PREFIX) :]
    if not BIN_NAME_PATTERN.fullmatch(name):
        raise ManifestError(
            f"{field} {token!r}: binary name must match {BIN_NAME_PATTERN.pattern}"
        )
    return name


class _StrictModel(BaseModel):
    model_config = ConfigDict(extra="forbid", str_strip_whitespace=True)


class PermissionRef(_StrictModel):
    """Object form of a permission entry: ``{id, required, degraded_behavior}``."""

    id: str
    required: bool = True
    degraded_behavior: str | None = None


def _normalize_permission(value: Any) -> dict[str, Any]:
    """Canonicalize a permission entry to dict shape (string OR object)."""
    if isinstance(value, str):
        return {"id": value, "required": True, "degraded_behavior": None}
    if isinstance(value, dict):
        return value
    raise ManifestError(f"permission entry must be str or object, got {type(value)}")
