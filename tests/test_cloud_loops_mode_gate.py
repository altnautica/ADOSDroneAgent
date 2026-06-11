"""Cloud relay loops gate on the same mode allowlist as the native path.

The in-process cloud loops (beacon / heartbeat / command-poll) are the
fallback path that runs only when the single-process runtime owns the
cloud relay. They must mirror the native gate: emit to Convex only when
``server.mode`` is ``cloud`` or ``self_hosted``. In ``local`` mode (the
install-time default) — or any unrecognized mode — they stay silent so a
LAN-paired agent never reaches out to the cloud backend.
"""

from __future__ import annotations

import asyncio

import pytest

from ados.core.config import ADOSConfig
from ados.core.main import (
    cloud_beacon_loop,
    cloud_command_poll_loop,
    cloud_heartbeat_loop,
)


class _OneShotShutdown:
    """Lets a ``while not _shutdown.is_set()`` body run exactly once.

    The first ``is_set()`` call (loop condition) returns ``False`` so the
    body executes; every later call returns ``True`` so the loop exits
    after a single iteration without driving real wall-clock sleeps.
    """

    def __init__(self) -> None:
        self._calls = 0

    def is_set(self) -> bool:
        self._calls += 1
        return self._calls > 1


class _RecordingPairingManager:
    """A paired manager whose cloud-touching calls are tracked, not real."""

    is_paired = True
    api_key = "test-key"

    def __init__(self) -> None:
        self.code_calls = 0

    def get_or_create_code(self) -> str:
        self.code_calls += 1
        return "CODE42"

    def generate_api_key(self) -> str:
        return "test-key"

    def code_expires_at(self):
        return None


class _FakeResponse:
    """A 200 response carrying an empty command list for the poll loop."""

    status_code = 200

    def json(self) -> dict:
        return {"commands": []}


def _fake_app(mode: str) -> ADOSConfig:
    """Build a minimal app stand-in carrying just what the loops read."""
    config = ADOSConfig()
    config.server.mode = mode
    config.pairing.convex_url = "https://convex.example.invalid"

    class _App:
        pass

    app = _App()
    app.config = config
    app._shutdown = _OneShotShutdown()
    app.pairing_manager = _RecordingPairingManager()
    app.discovery_service = None
    app.board_name = "test-board"
    return app


@pytest.mark.parametrize("loop", [
    cloud_beacon_loop,
    cloud_heartbeat_loop,
    cloud_command_poll_loop,
])
@pytest.mark.parametrize("mode", ["local", "bogus"])
async def test_cloud_loops_silent_in_local_and_unknown_mode(
    loop, mode, monkeypatch
) -> None:
    """No cloud loop POSTs/GETs when the server mode is not on the allowlist.

    The check is fail-loud independent of the loops' broad ``except
    Exception``: a recording client captures every network call into a list
    read AFTER the loop returns, so a regressed gate (a populated convex_url
    in local/bogus mode) surfaces as a non-empty record rather than as an
    exception the loop would swallow.
    """
    import httpx

    touched: list[str] = []

    class _RecordingClient:
        def __init__(self, *args, **kwargs) -> None:
            pass

        async def __aenter__(self):
            return self

        async def __aexit__(self, *exc) -> None:
            return None

        async def post(self, url, **kwargs):
            touched.append(url)
            return None

        async def get(self, url, **kwargs):
            touched.append(url)
            return _FakeResponse()

    monkeypatch.setattr(httpx, "AsyncClient", _RecordingClient)
    # Collapse the inter-iteration sleep so the single iteration is instant.
    monkeypatch.setattr(asyncio, "sleep", _noop_sleep)

    app = _fake_app(mode)
    # Give every loop what it needs to REACH the network so the mode allowlist
    # is the only thing that can keep it silent. The heartbeat loop builds its
    # payload before dialing; the beacon loop gates on the *unpaired* state.
    app._build_heartbeat_payload = lambda: {"deviceId": "abc"}
    if loop is cloud_beacon_loop:
        app.pairing_manager.is_paired = False

    await loop(app)
    assert not touched, f"{loop.__name__} reached the cloud in {mode} mode: {touched}"


@pytest.mark.parametrize("mode", ["cloud", "self_hosted"])
async def test_cloud_heartbeat_emits_for_allowlisted_modes(
    mode, monkeypatch
) -> None:
    """The heartbeat reaches the network when the mode is on the allowlist."""
    import httpx

    posted: list[str] = []

    class _RecordingClient:
        def __init__(self, *args, **kwargs) -> None:
            pass

        async def __aenter__(self):
            return self

        async def __aexit__(self, *exc) -> None:
            return None

        async def post(self, url, **kwargs):
            posted.append(url)
            return None

    monkeypatch.setattr(httpx, "AsyncClient", _RecordingClient)
    monkeypatch.setattr(asyncio, "sleep", _noop_sleep)

    app = _fake_app(mode)
    app._build_heartbeat_payload = lambda: {"deviceId": "abc"}

    await cloud_heartbeat_loop(app)
    assert posted, f"heartbeat did not POST in {mode} mode"


async def _noop_sleep(_seconds: float) -> None:
    """Stand-in for ``asyncio.sleep`` that yields without real delay."""
    return None
