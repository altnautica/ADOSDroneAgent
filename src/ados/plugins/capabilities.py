"""Canonical capability catalog for ADOS plugins.

Authoritative list of named capabilities a plugin manifest may
declare. Manifest validation accepts strings here as opaque
permission identifiers; per-capability enforcement gates land
incrementally as the surfaces they protect ship.

Enforcement lives with the native host, which gates every wire method
against the required cap in the generated dispatch table before the
handler runs. The caps with no gated method behind them yet are
recorded in plugin state and surfaced at install time; treat those as
advisory until the relevant subsystem ships its check.

The generated :data:`CAPABILITY_CATALOG` supplies each capability's
human-readable label, description, coarse category, and risk
classification. The native plugin lifecycle inlines the same metadata on
each declared permission for the install dialog; the local ``ados plugin
lint`` reads it from here.

The catalog data (:data:`AGENT_CAPABILITIES`,
:data:`ENFORCED_AGENT_CAPABILITIES`, :data:`CAPABILITY_CATALOG`) is
**generated** from ``crates/ados-protocol/capabilities.toml`` by the
``ados-capabilities-codegen`` tool, which emits the same catalog for
Python, Rust, and TypeScript so the three cannot drift. Edit the TOML
and regenerate; do not edit ``_capabilities_generated.py`` by hand.
This module adds the id-set helpers and the import-time self-check on top
of the generated data.
"""

from __future__ import annotations

from ados.plugins._capabilities_generated import (
    AGENT_CAPABILITIES,
    CAPABILITY_CATALOG,
    ENFORCED_AGENT_CAPABILITIES,
    GCS_CAPABILITIES,
)

__all__ = [
    "AGENT_CAPABILITIES",
    "ENFORCED_AGENT_CAPABILITIES",
    "GCS_CAPABILITIES",
    "CAPABILITY_CATALOG",
    "is_known_agent_capability",
    "is_known_gcs_capability",
]


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


def is_known_gcs_capability(cap: str) -> bool:
    """Return True if ``cap`` is a known GCS-half capability.

    The browser-side runtime owns the GCS catalog metadata; the agent
    keeps only the id set so a manifest's ``gcs`` block can warn (never
    reject) on an unrecognised GCS capability, symmetric with the
    agent-half check above.
    """
    return cap in GCS_CAPABILITIES
