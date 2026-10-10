"""Model download runs in the background: 202 + status URL, 409 while running."""

from __future__ import annotations

import asyncio

import pytest
from fastapi import HTTPException

import ados.api.routes.vision_models as route


class _Manager:
    def __init__(self, variant: dict | None) -> None:
        self.variant = variant
        self.release = asyncio.Event()
        self.started: list[str] = []

    async def fetch_registry(self) -> list:
        return []

    def select_best_variant(self, model_id: str) -> dict | None:
        return self.variant

    async def download_model(self, model_id: str) -> str:
        self.started.append(model_id)
        await self.release.wait()
        return f"/models/{model_id}.rknn"


@pytest.fixture
def manager(monkeypatch):
    holder: dict = {}

    def use(variant: dict | None) -> _Manager:
        mm = _Manager(variant)
        holder["app"] = type("App", (), {"model_manager": mm})()
        monkeypatch.setattr(route, "get_agent_app", lambda: holder["app"])
        monkeypatch.setattr(route, "_DOWNLOADS", {})
        return mm

    return use


async def test_download_returns_202_status_url_and_refuses_a_second(manager) -> None:
    mm = manager({"url": "https://models.example.com/yolo.rknn"})
    body = await route.download_vision_model("yolo")
    assert body["status_url"] == "/api/vision/models/yolo/status"
    await asyncio.sleep(0)
    assert mm.started == ["yolo"]

    with pytest.raises(HTTPException) as excinfo:
        await route.download_vision_model("yolo")
    assert excinfo.value.status_code == 409

    mm.release.set()
    await asyncio.sleep(0.01)
    assert "yolo" not in route._DOWNLOADS


async def test_download_errors_are_http_errors(manager) -> None:
    manager(None)
    with pytest.raises(HTTPException) as excinfo:
        await route.download_vision_model("missing")
    assert excinfo.value.status_code == 404

    manager({"url": ""})
    with pytest.raises(HTTPException) as excinfo:
        await route.download_vision_model("nourl")
    assert excinfo.value.status_code == 422


async def test_download_without_a_model_manager_is_503(monkeypatch) -> None:
    monkeypatch.setattr(route, "get_agent_app", lambda: object())
    with pytest.raises(HTTPException) as excinfo:
        await route.download_vision_model("yolo")
    assert excinfo.value.status_code == 503


def test_download_route_declares_202() -> None:
    match = [r for r in route.router.routes if getattr(r, "path", "") == "/vision/models/{model_id}/download"]
    assert match and match[0].status_code == 202
