"""Conformance RouteSpecs for the ground-station relay/receiver read routes.

The relay/receiver read routes read back the ``gs.relay_state`` /
``gs.receiver_state`` events the relay/receiver loops ship (the same body they
write to the ``wfb-relay.json`` / ``wfb-receiver.json`` sidecars), so one
RouteSpec per state covers the body each route slices.

The ``/status`` composite is mostly irreducibly live and gets no RouteSpec here:
its pair key, ``network`` (AP probe), ``system`` (psutil), ``gcs`` placeholder,
``recording`` / ``video``, ``role`` (role-file read), and ``link`` (a renamed
projection with its own file-mtime staleness flip) have no producer event. Its
one store-backable leg, the ``mesh`` sub-block, is covered by the ``mesh-state``
RouteSpec (it projects the same ``mesh.state`` event).
"""

from __future__ import annotations

from ..routes import FieldSpec, Locator, RouteSpec

# Every key in the relay state body the producer ships (the wfb-relay.json
# shape). The route returns this verbatim.
_RELAY_STATE_FIELDS = [
    "role",
    "drone_iface",
    "receiver_ip",
    "receiver_port",
    "receiver_last_seen_ms",
    "fragments_seen",
    "fragments_forwarded",
    "up",
    "mesh_iface",
]

# Every key in the receiver state body the producer ships (the wfb-receiver.json
# shape). The relays/combined routes project subsets of this; ``relays`` is the
# nested per-relay array.
_RECEIVER_STATE_FIELDS = [
    "role",
    "drone_iface",
    "listen_port",
    "accept_local_nic",
    "mesh_iface",
    "relays",
    "fragments_after_dedup",
    "fec_repaired",
    "output_kbps",
    "up",
]


def routes() -> list[RouteSpec]:
    """The relay-state + receiver-state route set."""
    return [_relay_state_route(), _receiver_state_route()]


def _relay_state_route() -> RouteSpec:
    """Full relay state body (live events, store-only).

    The /wfb/relay/status route reads this back instead of the sidecar file;
    every field is a key in the shipped detail map.
    """
    return RouteSpec(
        name="gs-relay-state",
        kind="events",
        logd_params={"kind": "events", "event_kind": "gs.relay_state", "limit": 200},
        observability_path="/api/v2/observability/events",
        row_match={"kind": "gs.relay_state", "source": "ados-groundlink"},
        fields=[
            FieldSpec(
                field=f,
                locator=Locator.DETAIL_KEY,
                classification="live",
                producer="ados-groundlink",
            )
            for f in _RELAY_STATE_FIELDS
        ],
    )


def _receiver_state_route() -> RouteSpec:
    """Full receiver state body (live events, store-only).

    The /wfb/receiver/relays + /wfb/receiver/combined routes project subsets of
    this body; every field is a key in the shipped detail map.
    """
    return RouteSpec(
        name="gs-receiver-state",
        kind="events",
        logd_params={
            "kind": "events",
            "event_kind": "gs.receiver_state",
            "limit": 200,
        },
        observability_path="/api/v2/observability/events",
        row_match={"kind": "gs.receiver_state", "source": "ados-groundlink"},
        fields=[
            FieldSpec(
                field=f,
                locator=Locator.DETAIL_KEY,
                classification="live",
                producer="ados-groundlink",
            )
            for f in _RECEIVER_STATE_FIELDS
        ],
    )
