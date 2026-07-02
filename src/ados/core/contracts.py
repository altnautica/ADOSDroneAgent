"""IPC contract + sidecar version registry for the ADOS Drone Agent.

The single source of truth for every inter-process wire-contract version
integer and every on-disk state-sidecar version. The data
(:data:`CONTRACT_VERSIONS`, :data:`SIDECAR_VERSIONS`, :data:`CONTRACT_CATALOG`,
:data:`SIDECAR_CATALOG`) is **generated** from
``crates/ados-protocol/contracts.toml`` by the ``ados-capabilities-codegen``
tool, which emits the same registry for Rust, Python, and TypeScript so the
three cannot drift. Edit the TOML and regenerate; do not edit
``_contracts_generated.py`` by hand. This module adds the lookup helpers and
the import-time self-check on top of the generated data.

The ``version`` for a ``status == "metadata"`` contract is informational
(``0`` means the contract has no registry-managed on-wire integer, e.g. a
transport primitive or a surface versioned externally).
"""

from __future__ import annotations

from ados.core._contracts_generated import (
    CONTRACT_CATALOG,
    CONTRACT_VERSIONS,
    SIDECAR_CATALOG,
    SIDECAR_VERSIONS,
)

__all__ = [
    "CONTRACT_VERSIONS",
    "SIDECAR_VERSIONS",
    "CONTRACT_CATALOG",
    "SIDECAR_CATALOG",
    "contract_version",
    "sidecar_version",
]


# Self-check at import time: the version map and the data catalog must cover
# exactly the same ids. Drift raises a stack trace at import rather than
# silently shipping a contract with a version but no metadata (or the reverse).
_contract_skew = set(CONTRACT_VERSIONS) ^ set(CONTRACT_CATALOG)
if _contract_skew:
    raise RuntimeError(
        "CONTRACT_VERSIONS and CONTRACT_CATALOG disagree on ids: "
        + ", ".join(sorted(_contract_skew))
    )
_sidecar_skew = set(SIDECAR_VERSIONS) ^ set(SIDECAR_CATALOG)
if _sidecar_skew:
    raise RuntimeError(
        "SIDECAR_VERSIONS and SIDECAR_CATALOG disagree on ids: "
        + ", ".join(sorted(_sidecar_skew))
    )
del _contract_skew, _sidecar_skew


def contract_version(contract_id: str) -> int | None:
    """Return the registered version of an IPC contract, or None if unknown."""
    return CONTRACT_VERSIONS.get(contract_id)


def sidecar_version(sidecar_id: str) -> int | None:
    """Return the registered version of a state sidecar, or None if unknown."""
    return SIDECAR_VERSIONS.get(sidecar_id)
