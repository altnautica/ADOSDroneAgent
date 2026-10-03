"""The pairing daemon runs the relay-side mesh join for the native front.

`POST /api/v1/ground-station/pair/join` forwards `{"op": "join"}` to the
daemon; the reply must carry the mesh on success and the join's own error
code on failure, because the front maps that code straight into its 503 body.
The invite it issues must name the receiver's configured listen port, and a
revoke reply must say whether the call actually revoked anything.
"""

from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace

import pytest

from ados.services.ground_station import pairing_client, pairing_daemon
from ados.services.ground_station import pairing_manager as pm


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


def test_the_invite_names_the_configured_receiver_port(tmp_path: Path, monkeypatch):
    mesh_id = tmp_path / "mesh-id"
    mesh_id.write_text("mesh-7\n", encoding="utf-8")
    psk = tmp_path / "mesh.psk"
    psk.write_bytes(b"secret-psk")
    config = SimpleNamespace(
        ground_station=SimpleNamespace(
            mesh=SimpleNamespace(shared_key_path=str(psk)),
            wfb_receiver=SimpleNamespace(listen_port=6123),
        ),
        video=SimpleNamespace(wfb=SimpleNamespace(channel=149)),
    )
    monkeypatch.setattr("ados.core.config.load_config", lambda: config)
    monkeypatch.setattr(pairing_daemon, "MESH_ID_PATH", mesh_id)
    monkeypatch.setattr(
        "ados.services.wfb.key_mgr.get_key_paths",
        lambda: (tmp_path / "tx.key", tmp_path / "rx.key"),
    )

    bundle = pairing_daemon._build_invite_bundle()

    assert bundle is not None
    assert bundle.receiver_mdns_port == 6123


@pytest.mark.asyncio
async def test_revoking_an_already_revoked_relay_reports_no_change(
    tmp_path: Path, monkeypatch
):
    monkeypatch.setattr(pm, "REVOCATIONS_PATH", tmp_path / "revocations.json")
    monkeypatch.setattr(pm, "_REVOCATIONS_CACHE", None)
    monkeypatch.setattr(pm, "_REVOCATIONS_CACHE_TS_NS", 0)

    first = await pairing_daemon._handle_op("revoke", {"device_id": "relay-9"})
    second = await pairing_daemon._handle_op("revoke", {"device_id": "relay-9"})

    assert first == {"ok": True, "result": {"revoked": True}}
    assert second == {"ok": True, "result": {"revoked": False}}
    assert pm.is_revoked("relay-9")
