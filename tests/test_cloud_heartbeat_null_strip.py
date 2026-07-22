"""The in-process cloud heartbeat strips top-level nulls before the POST.

The status receiver's validator types its optional root fields as
"absent OR T" — an explicit JSON ``null`` is rejected, so one ``None``
leaf (an offload target, an FC hint) would fail the whole heartbeat.
The native relay omits every ``None``-valued root key; the packaged
loop must do the same. Nested block members are NOT stripped: the radio
block's three-state verdicts serialize ``null`` deliberately (declared
nullable on the validator) so "no reading" never collapses into a
fabricated absent-key state.
"""

from __future__ import annotations

import asyncio

from ados.core.config import ADOSConfig
from ados.core.main import cloud_heartbeat_loop


class _OneShotShutdown:
    """Lets a ``while not _shutdown.is_set()`` body run exactly once."""

    def __init__(self) -> None:
        self._calls = 0

    def is_set(self) -> bool:
        self._calls += 1
        return self._calls > 1


class _PairedManager:
    is_paired = True
    api_key = "test-key"


async def _noop_sleep(_seconds: float) -> None:
    return None


def _fake_app(payload: dict):
    config = ADOSConfig()
    config.server.mode = "cloud"
    config.pairing.convex_url = "https://convex.example.invalid"

    class _App:
        pass

    app = _App()
    app.config = config
    app._shutdown = _OneShotShutdown()
    app.pairing_manager = _PairedManager()
    app._build_heartbeat_payload = lambda: payload
    return app


async def test_heartbeat_wire_payload_carries_no_top_level_nulls(
    monkeypatch,
) -> None:
    """None-valued root keys are omitted; a nested radio verdict null survives."""
    import httpx

    sent: list[dict] = []

    class _RecordingClient:
        def __init__(self, *args, **kwargs) -> None:
            pass

        async def __aenter__(self):
            return self

        async def __aexit__(self, *exc) -> None:
            return None

        async def post(self, url, **kwargs):
            sent.append(kwargs["json"])
            return None

    monkeypatch.setattr(httpx, "AsyncClient", _RecordingClient)
    monkeypatch.setattr(asyncio, "sleep", _noop_sleep)

    app = _fake_app(
        {
            "deviceId": "abc",
            "version": "1.0.0",
            "uptimeSeconds": 5,
            # Always-present builder keys that read None on a bench node.
            "temperature": None,
            "fcVariant": None,
            "perceptionOffloadTarget": None,
            "wfbAdapterInjectionOk": None,
            # A nested block whose null members are load-bearing verdicts.
            "radio": {"state": "absent", "adapter_injection_ok": None},
        }
    )

    await cloud_heartbeat_loop(app)

    assert sent, "heartbeat did not POST"
    wire = sent[0]
    # No top-level key may carry an explicit null on the wire.
    nulls = [k for k, v in wire.items() if v is None]
    assert not nulls, f"top-level nulls reached the wire: {nulls}"
    for stripped in (
        "temperature",
        "fcVariant",
        "perceptionOffloadTarget",
        "wfbAdapterInjectionOk",
    ):
        assert stripped not in wire
    # Non-null keys pass through untouched.
    assert wire["deviceId"] == "abc"
    assert wire["uptimeSeconds"] == 5
    # The nested verdict null survives — it is a declared-nullable
    # "no reading" state, not a strippable absence.
    assert wire["radio"] == {"state": "absent", "adapter_injection_ok": None}
