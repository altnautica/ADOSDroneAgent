"""Cloud setup fixes: tunnel restart on token rotation, reboot fallback, log unit."""

from __future__ import annotations

import asyncio
from types import SimpleNamespace

import ados.setup.service._cloud_actions as actions

_TOKEN = "eyJhIjoiZXhhbXBsZS10dW5uZWwtdG9rZW4tdmFsdWUifQ"


def _runtime(tmp_path, *, provider: str, enabled: bool) -> SimpleNamespace:
    cf = SimpleNamespace(
        enabled=enabled,
        token_path=str(tmp_path / "token"),
        service_name="cloudflared",
    )
    return SimpleNamespace(
        config=SimpleNamespace(remote_access=SimpleNamespace(provider=provider, cloudflare=cf))
    )


def test_token_rotation_restarts_an_enabled_tunnel(tmp_path, monkeypatch) -> None:
    calls: list[str] = []
    monkeypatch.setattr(actions, "persist_config", lambda *a, **k: None)
    monkeypatch.setattr(
        actions.remote_access_sync,
        "restart_tunnel_unit",
        lambda unit: calls.append(unit) or {"unit": unit, "ok": True, "error": None},
        raising=False,
    )
    result = actions.install_cloudflare_token(
        _runtime(tmp_path, provider="cloudflare", enabled=True), _TOKEN
    )
    assert result.ok is True
    assert calls == ["cloudflared"]
    assert result.data["tunnel_restart"]["ok"] is True
    assert "service_command" not in result.data


def test_first_token_install_leaves_the_start_to_the_posture_sync(tmp_path, monkeypatch) -> None:
    calls: list[str] = []
    monkeypatch.setattr(actions, "persist_config", lambda *a, **k: None)
    monkeypatch.setattr(
        actions.remote_access_sync,
        "restart_tunnel_unit",
        lambda unit: calls.append(unit) or {"unit": unit, "ok": True, "error": None},
        raising=False,
    )
    result = actions.install_cloudflare_token(
        _runtime(tmp_path, provider="none", enabled=False), _TOKEN
    )
    assert result.ok is True
    assert calls == []


def test_a_failed_restart_is_reported(tmp_path, monkeypatch) -> None:
    monkeypatch.setattr(actions, "persist_config", lambda *a, **k: None)
    monkeypatch.setattr(
        actions.remote_access_sync,
        "restart_tunnel_unit",
        lambda unit: {"unit": unit, "ok": False, "error": "unit not found"},
        raising=False,
    )
    result = actions.install_cloudflare_token(
        _runtime(tmp_path, provider="cloudflare", enabled=True), _TOKEN
    )
    assert result.data["tunnel_restart"]["ok"] is False
    assert "unit not found" in result.message


def test_log_stream_only_tails_a_cloudflared_unit() -> None:
    from ados.api.routes.setup.cloud import _log_unit

    assert _log_unit("cloudflared-tunnel") == "cloudflared-tunnel"
    assert _log_unit("ssh") == "cloudflared"
    assert _log_unit("cloudflared; rm") == "cloudflared"
    assert _log_unit(None) == "cloudflared"
    assert _log_unit("  ") == "cloudflared"


def test_reboot_falls_back_when_systemctl_fails(monkeypatch) -> None:
    import ados.api.routes.setup.cloud as cloud

    ran: list[list[str]] = []

    class _Proc:
        def __init__(self, code: int) -> None:
            self.code = code

        async def wait(self) -> int:
            return self.code

    async def fake_exec(*cmd, **_kw):
        ran.append(list(cmd))
        return _Proc(1 if cmd[0] == "systemctl" else 0)

    async def no_sleep(_s: float) -> None:
        return None

    monkeypatch.setattr(cloud.asyncio, "create_subprocess_exec", fake_exec)
    monkeypatch.setattr(cloud.asyncio, "sleep", no_sleep)
    asyncio.run(cloud._reboot_after_delay(0))
    assert ran == [["systemctl", "reboot"], ["/sbin/reboot"]]


def test_poweroff_falls_back_when_systemctl_fails(monkeypatch) -> None:
    import ados.api.routes.setup.cloud as cloud

    ran: list[list[str]] = []

    class _Proc:
        async def wait(self) -> int:
            return 1 if ran[-1][0] == "systemctl" else 0

    async def fake_exec(*cmd, **_kw):
        ran.append(list(cmd))
        return _Proc()

    async def no_sleep(_s: float) -> None:
        return None

    monkeypatch.setattr(cloud.asyncio, "create_subprocess_exec", fake_exec)
    monkeypatch.setattr(cloud.asyncio, "sleep", no_sleep)
    asyncio.run(cloud._poweroff_after_delay(0))
    assert ran == [["systemctl", "poweroff"], ["/sbin/poweroff"]]


def test_shutdown_route_schedules_poweroff(monkeypatch) -> None:
    from fastapi import FastAPI
    from fastapi.testclient import TestClient

    import ados.api.routes.setup as setup_routes
    import ados.api.routes.setup.cloud as cloud

    called: list[float] = []

    def _stub(seconds: float):
        called.append(seconds)
        return asyncio.sleep(0)

    monkeypatch.setattr(cloud, "_poweroff_after_delay", _stub)
    app = FastAPI()
    app.include_router(setup_routes.router)
    with TestClient(app) as client:
        resp = client.post("/v1/setup/shutdown")
    assert resp.status_code == 200
    assert resp.json()["ok"] is True
    assert called == [3.0]
