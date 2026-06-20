"""Tests for the FC-liveness view on the MAVLink IPC state shim.

The native router publishes a gated `fc_connected` (transport open AND a fresh
HEARTBEAT) plus the `transport_open` / `mavlink_alive` / `heartbeat_age_s` /
`fc_source` split on the state snapshot. ``IpcFcConnection`` exposes those to the
Python readers (the cloud heartbeat, the MQTT gateway) and ``IpcVehicleState``
strips them from the telemetry projection.
"""

from __future__ import annotations

from ados.services.mavlink.ipc_state import IpcFcConnection, IpcVehicleState


def _vs(snapshot: dict) -> IpcVehicleState:
    vs = IpcVehicleState()
    vs.update_from_dict(snapshot)
    return vs


def test_fc_connection_reads_the_gated_truth_and_the_split() -> None:
    # The exact bug it guards: transport open but MAVLink not alive → NOT
    # connected.
    fc = IpcFcConnection(
        _vs(
            {
                "fc_connected": False,
                "transport_open": True,
                "mavlink_alive": False,
                "heartbeat_age_s": 12.0,
                "fc_source": "serial",
                "fc_port": "/dev/ttyACM0",
                "fc_baud": 115200,
            }
        )
    )
    assert fc.connected is False
    assert fc.transport_open is True
    assert fc.mavlink_alive is False
    assert fc.heartbeat_age_s == 12.0
    assert fc.source == "serial"
    assert fc.port == "/dev/ttyACM0"
    assert fc.baud == 115200


def test_fc_connection_connected_when_alive() -> None:
    fc = IpcFcConnection(
        _vs(
            {
                "fc_connected": True,
                "transport_open": True,
                "mavlink_alive": True,
                "heartbeat_age_s": 0.4,
            }
        )
    )
    assert fc.connected is True
    assert fc.mavlink_alive is True


def test_fc_connection_defaults_on_an_empty_snapshot() -> None:
    fc = IpcFcConnection(IpcVehicleState())
    assert fc.connected is False
    assert fc.transport_open is False
    assert fc.mavlink_alive is False
    assert fc.heartbeat_age_s is None
    assert fc.source == "auto"


def test_telemetry_projection_strips_the_liveness_extras() -> None:
    vs = _vs(
        {
            "armed": True,
            "mode": "GUIDED",
            "fc_connected": True,
            "transport_open": True,
            "mavlink_alive": True,
            "heartbeat_age_s": 0.5,
            "fc_source": "serial",
        }
    )
    out = vs.to_dict()
    assert out["armed"] is True
    assert out["mode"] == "GUIDED"
    for key in (
        "fc_connected",
        "transport_open",
        "mavlink_alive",
        "heartbeat_age_s",
        "fc_source",
    ):
        assert key not in out, f"{key} must be stripped from the telemetry view"
