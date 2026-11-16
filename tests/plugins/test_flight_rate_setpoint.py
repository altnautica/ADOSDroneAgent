"""``ctx.flight.rate_setpoint`` sends the plugin's thrust and nothing else."""

from __future__ import annotations

import asyncio

from ados.plugins.ipc.context import _FlightClient

THROTTLE_IGNORE = 64
RATES_IGNORED = 0b111


class _FakeIpc:
    def __init__(self) -> None:
        self.sent: list[dict] = []

    async def flight_rate_setpoint_send(self, setpoint: dict) -> dict:
        self.sent.append(setpoint)
        return {"sent": True}


def test_the_thrust_is_sent_unchanged() -> None:
    ipc = _FakeIpc()
    reply = asyncio.run(_FlightClient(ipc).rate_setpoint(type_mask=RATES_IGNORED, thrust=0.37))
    assert reply == {"sent": True}
    assert ipc.sent[0]["thrust"] == 0.37
    assert ipc.sent[0]["type_mask"] == RATES_IGNORED


def test_an_omitted_thrust_is_left_to_the_host_not_defaulted() -> None:
    # The host requires a thrust unless the mask ignores throttle, so the
    # facade never fills one in: zero thrust would idle the motors.
    ipc = _FakeIpc()
    asyncio.run(_FlightClient(ipc).rate_setpoint(type_mask=RATES_IGNORED | THROTTLE_IGNORE))
    assert "thrust" not in ipc.sent[0]
