"""Mesh-state read-source helpers, sourced from the durable logging store.

The ground-station mesh read routes (``/mesh``, ``/mesh/neighbors``,
``/mesh/routes``, ``/mesh/gateways``) used to read their data straight off the
``/run/ados/mesh-state.json`` sidecar the native mesh poll loop writes. The
relay/receiver poll loop (``ados-groundlink``) now also ships the same full
snapshot body to the durable store as a ``mesh.state`` event, so these helpers
read that back instead. The route reads the store first and falls back to the
sidecar file, so losing the store degrades to the old behavior, never to a 500.

All four routes slice the one snapshot, so one source helper fetches the body and
small pure slicers project it the same way the live route does. There is no
staleness flip on the live mesh read (``_read_json_or_empty`` returns the file
content as-is), so this source has no event-age logic either — adding one would
break parity with the live path.
"""

from __future__ import annotations

from typing import Any

from ados.api.telemetry_source import query_rows


async def latest_mesh_snapshot() -> dict[str, Any] | None:
    """Most-recent full mesh-state body from the store, or ``None``.

    Reads the newest ``mesh.state`` event the relay/receiver poll loop shipped
    and returns its detail map (the same body written to ``mesh-state.json``).
    ``None`` when the store is unreachable or holds no ``mesh.state`` event, so
    the route falls back to the sidecar file.
    """
    rows = await query_rows("events", 1, event_kind="mesh.state")
    if not rows:
        return None
    row = rows[0]
    if not isinstance(row, dict):
        return None
    detail = row.get("detail")
    if not isinstance(detail, dict) or not detail:
        return None
    return detail


def slice_neighbors(detail: dict[str, Any]) -> dict[str, Any]:
    """Project the ``/mesh/neighbors`` shape from a stored snapshot body."""
    return {"neighbors": detail.get("neighbors", [])}


def slice_routes(detail: dict[str, Any]) -> dict[str, Any]:
    """Project the ``/mesh/routes`` shape from a stored snapshot body.

    Routes are aliased to neighbors on the live path today, so this preserves
    the same neighbors-under-``routes`` mapping for byte-identical parity.
    """
    return {"routes": detail.get("neighbors", [])}


def slice_gateways(detail: dict[str, Any]) -> dict[str, Any]:
    """Project the ``/mesh/gateways`` shape from a stored snapshot body."""
    return {
        "gateways": detail.get("gateways", []),
        "selected": detail.get("selected_gateway"),
    }


__all__ = [
    "latest_mesh_snapshot",
    "slice_neighbors",
    "slice_routes",
    "slice_gateways",
]
