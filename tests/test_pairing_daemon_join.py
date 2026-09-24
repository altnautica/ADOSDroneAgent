"""The pairing daemon runs the relay-side mesh join for the native front.

`POST /api/v1/ground-station/pair/join` forwards `{"op": "join"}` to the
daemon; the reply must carry the mesh on success and the join's own error
code on failure, because the front maps that code straight into its 503 body.
"""

from __future__ import annotations

from types import SimpleNamespace

import pytest

from ados.services.ground_station import pairing_client, pairing_daemon


@pytest.mark.asyncio
async def test_join_returns_the_mesh_the_invite_named(monkeypatch):
    seen: dict = {}

    async def fake_join(*, code, receiver_host, receiver_port):
        seen.update(code=code, host=receiver_host, port=receiver_port)
        return SimpleNamespace(
            ok=True, mesh_id="mesh-1", receiver_host="gs-a.local",
            error_code=None, error_message=None,
        )

    monkeypatch.setattr(pairing_client, "request_join", fake_join)
    reply = await pairing_daemon._handle_op(
        "join", {"code": "123456", "receiver_host": "", "receiver_port": 5801}
    )
    assert reply == {
        "ok": True,
        "result": {"mesh_id": "mesh-1", "receiver_host": "gs-a.local"},
    }
    assert seen == {"code": "123456", "host": None, "port": 5801}


@pytest.mark.asyncio
async def test_a_failed_join_keeps_its_error_code(monkeypatch):
    async def fake_join(**_):
        return SimpleNamespace(
            ok=False, mesh_id=None, receiver_host=None,
            error_code="E_INVITE_DECRYPT", error_message="wrong code",
        )

    monkeypatch.setattr(pairing_client, "request_join", fake_join)
    reply = await pairing_daemon._handle_op("join", {"code": "000000"})
    assert reply == {
        "ok": False,
        "error": "wrong code",
        "error_code": "E_INVITE_DECRYPT",
    }
