"""Canonical capability catalog for ADOS plugins.

Authoritative list of named capabilities a plugin manifest may
declare. Manifest validation accepts strings here as opaque
permission identifiers; per-capability enforcement gates land
incrementally as the surfaces they protect ship.

Today ``event.publish`` and ``event.subscribe`` are enforced (see
:mod:`ados.plugins.events`). The rest are recorded in plugin state
and surfaced in the install dialog risk badge, but no runtime gate
rejects an action because of them yet. Treat the un-enforced caps
as advisory until the relevant subsystem ships its check.

Each capability ID also has a :class:`CapabilityMeta` entry in
:data:`CAPABILITY_CATALOG`. The catalog supplies human-readable
labels, descriptions, a coarse category, and a risk classification
that the install dialog renders. The REST endpoints (parse +
install) inline this metadata on each declared permission so the
GCS does not need to ship a parallel copy of the agent's catalog.

The catalog data (:data:`AGENT_CAPABILITIES`,
:data:`ENFORCED_AGENT_CAPABILITIES`, :data:`CAPABILITY_CATALOG`) is
**generated** from ``crates/ados-protocol/capabilities.toml`` by the
``ados-capabilities-codegen`` tool, which emits the same catalog for
Python, Rust, and TypeScript so the three cannot drift. Edit the TOML
and regenerate; do not edit ``_capabilities_generated.py`` by hand.
This module adds the type, the helpers, and the import-time
self-check on top of the generated data.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Optional, TypedDict

from ados.plugins._capabilities_generated import (
    AGENT_CAPABILITIES,
    CAPABILITY_CATALOG,
    ENFORCED_AGENT_CAPABILITIES,
)
from ados.plugins.errors import CapabilityDenied

if TYPE_CHECKING:
    from ados.plugins.supervisor import PluginSupervisor

__all__ = [
    "AGENT_CAPABILITIES",
    "ENFORCED_AGENT_CAPABILITIES",
    "CAPABILITY_CATALOG",
    "CapabilityMeta",
    "is_known_agent_capability",
    "get_capability_meta",
    "is_known_capability",
    "get_granted_caps",
    "has_capability",
    "require_capability",
]


class CapabilityMeta(TypedDict):
    """Human-readable metadata for one capability.

    ``label`` is a short action-verb sentence rendered as the
    permission row title in the install dialog. ``description`` is
    the body paragraph that explains what the permission unlocks and
    why an operator might care. ``category`` and ``risk`` drive
    grouping and the risk badge.
    """

    label: str
    description: str
    category: str  # "hardware" | "flight_control" | "data_network"
    #                 | "compute_process" | "ui_slot"
    risk: str  # "low" | "medium" | "high" | "critical"
    risk_reason: str


# Self-check at import time: catalog completeness. The frozen
# capability set is the single source of truth, and every id in it
# must have a catalog entry. Drift produces a stack trace at import
# rather than silently shipping a permission with no human label.
_missing_catalog_entries = AGENT_CAPABILITIES - CAPABILITY_CATALOG.keys()
if _missing_catalog_entries:
    raise RuntimeError(
        "AGENT_CAPABILITIES missing CAPABILITY_CATALOG entries: "
        + ", ".join(sorted(_missing_catalog_entries))
    )
_orphan_catalog_entries = CAPABILITY_CATALOG.keys() - AGENT_CAPABILITIES
if _orphan_catalog_entries:
    raise RuntimeError(
        "CAPABILITY_CATALOG has entries not in AGENT_CAPABILITIES: "
        + ", ".join(sorted(_orphan_catalog_entries))
    )
del _missing_catalog_entries, _orphan_catalog_entries


def is_known_agent_capability(cap: str) -> bool:
    """Return True if the capability is declared in the catalog."""
    return cap in AGENT_CAPABILITIES


def get_capability_meta(cap_id: str) -> Optional[CapabilityMeta]:
    """Return the catalog entry for ``cap_id`` or ``None`` if unknown."""
    return CAPABILITY_CATALOG.get(cap_id)


def is_known_capability(cap_id: str) -> bool:
    """Return True if ``cap_id`` has a catalog entry.

    Equivalent to :func:`is_known_agent_capability` today since the
    catalog and the frozen capability set are kept in sync at import
    time, but exposed under a neutral name so REST callers that look
    up arbitrary ids do not have to import the agent-specific helper.
    """
    return cap_id in CAPABILITY_CATALOG


def get_granted_caps(
    supervisor: "PluginSupervisor", plugin_id: str
) -> set[str]:
    """Return the set of capability ids currently granted to plugin_id.

    Looks up the plugin's install record in supervisor state and
    extracts the granted permissions. Returns an empty set if the
    plugin is not installed.

    The supervisor IPC server already holds a verified
    :class:`CapabilityToken` per request and checks against
    ``token.granted_caps`` directly for the hot path. This helper is
    for non-IPC contexts (CLI, REST handlers, supervisor methods)
    that need the same view from state.
    """
    install = supervisor.find_install(plugin_id)
    if install is None:
        return set()
    return {pid for pid, grant in install.permissions.items() if grant.granted}


def has_capability(
    supervisor: "PluginSupervisor", plugin_id: str, cap: str
) -> bool:
    """Return True if ``cap`` is currently granted to ``plugin_id``."""
    return cap in get_granted_caps(supervisor, plugin_id)


def require_capability(
    supervisor: "PluginSupervisor", plugin_id: str, cap: str
) -> None:
    """Raise :class:`CapabilityDenied` if ``cap`` is not granted to ``plugin_id``."""
    if not has_capability(supervisor, plugin_id, cap):
        raise CapabilityDenied(plugin_id=plugin_id, capability=cap)
