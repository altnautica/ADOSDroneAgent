"""Ground-station status + relay/receiver read-source helpers.

The relay/receiver read routes (``/wfb/relay/status``, ``/wfb/receiver/relays``,
``/wfb/receiver/combined``) used to read their data straight off the
``/run/ados/wfb-relay.json`` / ``/run/ados/wfb-receiver.json`` sidecars the
native relay/receiver loops write. Those loops (``ados-groundlink``) now also
ship the same full state body to the durable store as a ``gs.relay_state`` /
``gs.receiver_state`` event, so these helpers read that back instead. The route
reads the store first and falls back to the sidecar file, so losing the store
degrades to the old behavior, never to a 500.

The ``/status`` composite is mostly irreducibly live (the AP probe, the pair
state, psutil, the recorder, the role-file read) and stays live. Its ``mesh``
sub-block is the one store-backable leg here: it projects the same
``mesh-state.json`` snapshot the relay/receiver poll loop now also emits as a
``mesh.state`` event, so it is served store-first from that event with a sidecar
fallback. The ``link`` sub-block is a renamed/reduced projection with its own
file-mtime staleness flip and stays live this pass.
"""

from __future__ import annotations

from typing import Any

from ados.api.sources.mesh import latest_mesh_snapshot
from ados.api.telemetry_source import query_rows


async def latest_relay_state() -> dict[str, Any] | None:
    """Most-recent full relay-state body from the store, or ``None``.

    Reads the newest ``gs.relay_state`` event the relay loop shipped and returns
    its detail map (the same body written to ``wfb-relay.json``). ``None`` when
    the store is unreachable or holds no such event, so the route falls back to
    the sidecar file.
    """
    return await _latest_event_detail("gs.relay_state")


async def latest_receiver_state() -> dict[str, Any] | None:
    """Most-recent full receiver-state body from the store, or ``None``.

    Reads the newest ``gs.receiver_state`` event the receiver loop shipped and
    returns its detail map (the same body written to ``wfb-receiver.json``).
    The route projects the relays / combined slices below. ``None`` when the
    store is unreachable or holds no such event, so the route falls back to the
    sidecar file.
    """
    return await _latest_event_detail("gs.receiver_state")


def slice_receiver_relays(detail: dict[str, Any]) -> dict[str, Any]:
    """Project the ``/wfb/receiver/relays`` shape from a stored receiver body."""
    return {"relays": detail.get("relays", [])}


def slice_receiver_combined(detail: dict[str, Any]) -> dict[str, Any]:
    """Project the ``/wfb/receiver/combined`` shape from a stored receiver body.

    Applies the same per-key defaults as the live route so an omitted key
    coalesces identically whether it is absent from the stored detail or the
    sidecar.
    """
    return {
        "fragments_after_dedup": detail.get("fragments_after_dedup", 0),
        "fec_repaired": detail.get("fec_repaired", 0),
        "output_kbps": detail.get("output_kbps", 0),
        "up": detail.get("up", False),
    }


async def latest_status_mesh_block() -> dict[str, Any] | None:
    """The ``/status`` ``mesh`` sub-block, sourced from the ``mesh.state`` event.

    Projects the same five fields the live ``/status`` route reads off
    ``mesh-state.json`` (``up``, ``peer_count`` = neighbor count,
    ``selected_gateway``, ``partition``, ``mesh_id``). ``None`` when the store is
    unreachable or holds no ``mesh.state`` event, so the route falls back to the
    sidecar read. Mirrors the live ``bool(...)`` / ``len(...)`` coercions exactly.
    """
    detail = await latest_mesh_snapshot()
    if detail is None:
        return None
    return {
        "up": bool(detail.get("up", False)),
        "peer_count": len(detail.get("neighbors", [])),
        "selected_gateway": detail.get("selected_gateway"),
        "partition": bool(detail.get("partition", False)),
        "mesh_id": detail.get("mesh_id"),
    }


async def _latest_event_detail(event_kind: str) -> dict[str, Any] | None:
    """Return the newest event's non-empty detail map for ``event_kind``."""
    rows = await query_rows("events", 1, event_kind=event_kind)
    if not rows:
        return None
    row = rows[0]
    if not isinstance(row, dict):
        return None
    detail = row.get("detail")
    if not isinstance(detail, dict) or not detail:
        return None
    return detail


__all__ = [
    "latest_relay_state",
    "latest_receiver_state",
    "slice_receiver_relays",
    "slice_receiver_combined",
    "latest_status_mesh_block",
]
