"""Conformance RouteSpecs for the mesh read routes.

The four mesh read routes (``/mesh``, ``/mesh/neighbors``, ``/mesh/routes``,
``/mesh/gateways``) all slice the one ``mesh.state`` event the relay/receiver
poll loop ships to the store (the same body it writes to ``mesh-state.json``),
so one RouteSpec covers the snapshot body the routes read back.

The ``/role`` and ``/mesh/config`` routes are NOT here: they are served live from
the role sentinel file + Pydantic config + systemd unit-name constants, not from
any sampled snapshot, so there is no producer event to assert against.
"""

from __future__ import annotations

from ..routes import FieldSpec, Locator, RouteSpec

# Every top-level key in the mesh snapshot body the producer ships. The nested
# ``neighbors[].*`` / ``gateways[].*`` sub-keys are not asserted as separate
# FieldSpecs (the harness checks top-level detail-key presence; the producer's
# round-trip test guarantees nested array fidelity), matching the wfb-status spec.
_MESH_STATE_FIELDS = [
    "role",
    "bat_iface",
    "mesh_iface",
    "carrier",
    "mesh_id",
    "up",
    "neighbors",
    "gateways",
    "selected_gateway",
    "partition",
    "started_at_ms",
    "last_poll_ms",
]


def routes() -> list[RouteSpec]:
    """The mesh-state route set (one event covers the four slice routes)."""
    return [_mesh_state_route()]


def _mesh_state_route() -> RouteSpec:
    """Full mesh snapshot body (live events, store-only).

    The /mesh family reads this back instead of the sidecar file; every field is
    a key in the shipped detail map. Filtered to the ground-side source so a
    single-profile rig reports it cleanly.
    """
    return RouteSpec(
        name="mesh-state",
        kind="events",
        logd_params={"kind": "events", "event_kind": "mesh.state", "limit": 200},
        observability_path="/api/v2/observability/events",
        row_match={"kind": "mesh.state", "source": "ados-groundlink"},
        fields=[
            FieldSpec(
                field=f,
                locator=Locator.DETAIL_KEY,
                classification="live",
                producer="ados-groundlink",
            )
            for f in _MESH_STATE_FIELDS
        ],
    )
