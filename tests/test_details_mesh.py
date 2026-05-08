"""Tests for the Mesh drilldown detail page."""

from __future__ import annotations

import asyncio
from typing import Any

import pytest
import structlog
from PIL import Image

from ados.services.ui.pages import PageContext, PageNavigator
from ados.services.ui.pages.details.mesh import MeshDetailPage
from ados.services.ui.theme import DARK
from ados.services.ui.touch.events import TouchGesture


class _Resp:
    def __init__(self, status_code: int, payload: Any) -> None:
        self.status_code = status_code
        self._payload = payload

    def json(self) -> Any:
        return self._payload


class _StubClient:
    def __init__(self) -> None:
        self.posts: list[tuple[str, Any]] = []

    async def post(
        self,
        url: str,
        *,
        json: Any | None = None,
        timeout: float = 2.0,
    ) -> _Resp:
        self.posts.append((url, json))
        return _Resp(200, {"ok": True})


class _StubDashboard:
    id = "dashboard"
    refresh_hz = 5.0
    async def on_enter(self, ctx: PageContext) -> None: ...
    async def on_leave(self, ctx: PageContext) -> None: ...
    async def render(self, ctx: PageContext) -> Image.Image:
        return Image.new("RGB", (480, 244), ctx.palette.bg_primary)
    def hit_zones(self, ctx: PageContext) -> list[Any]: return []
    async def on_touch(self, ctx: PageContext, zone: Any, g: TouchGesture) -> None: ...


def _ctx(state: dict, http: Any | None = None) -> PageContext:
    nav = PageNavigator(registry={"dashboard": _StubDashboard()})
    return PageContext(
        state=state,
        palette=DARK,
        hostname="groundnode",
        http=http,
        framebuffer=None,
        navigator=nav,
        logger=structlog.get_logger("test.mesh"),
    )


def _tap(x: int, y: int) -> TouchGesture:
    return TouchGesture(
        kind="tap",
        start_x=x,
        start_y=y,
        end_x=x,
        end_y=y,
        start_t_ms=0,
        end_t_ms=10,
        duration_ms=10,
        direction=None,
        velocity_px_per_s=0.0,
        samples=((x, y, 0),),
    )


@pytest.mark.asyncio
async def test_mesh_direct_role_renders_not_a_mesh_node() -> None:
    state = {"role": {"current": "direct", "mesh_capable": False}, "mesh": {}}
    page = MeshDetailPage()
    ctx = _ctx(state)
    await page.on_enter(ctx)
    img = await page.render(ctx)
    assert img.size == (480, 244)


@pytest.mark.asyncio
async def test_mesh_relay_role_renders_peer_list() -> None:
    state = {
        "role": {"current": "relay", "mesh_capable": True},
        "mesh": {
            "up": True,
            "peers": [
                {"device_id": "abcd1234efgh", "role": "receiver", "last_seen_seconds_ago": 4},
                {"device_id": "ijkl5678mnop", "role": "relay", "last_seen_seconds_ago": 12},
            ],
            "selected_gateway": "gn-2",
            "partition": False,
        },
    }
    page = MeshDetailPage()
    ctx = _ctx(state)
    await page.on_enter(ctx)
    img = await page.render(ctx)
    assert img.size == (480, 244)


@pytest.mark.asyncio
async def test_mesh_receiver_renders_when_mesh_down() -> None:
    state = {
        "role": {"current": "receiver", "mesh_capable": True},
        "mesh": {"up": False, "peers": []},
    }
    page = MeshDetailPage()
    ctx = _ctx(state)
    await page.on_enter(ctx)
    img = await page.render(ctx)
    assert img.size == (480, 244)


@pytest.mark.asyncio
async def test_mesh_switch_role_post_fires_setup_profile() -> None:
    state = {"role": {"current": "relay", "mesh_capable": True}, "mesh": {"up": True, "peers": []}}
    client = _StubClient()
    ctx = _ctx(state, client)
    page = MeshDetailPage()
    await page.on_enter(ctx)
    await page.render(ctx)
    # Open the picker first.
    switch_zone = next(z for z in page.hit_zones(ctx) if z.id == "mesh.switch_role")
    await page.on_touch(ctx, switch_zone, _tap(420, 56))
    assert page._picker_open
    # Re-render so the picker zones come up.
    await page.render(ctx)
    receiver_zone = next(z for z in page.hit_zones(ctx) if z.id == "mesh.role.receiver")
    await page.on_touch(ctx, receiver_zone, _tap(240, 220))
    # Allow the scheduled task to actually run.
    if page._switch_in_flight is not None:
        await page._switch_in_flight
    assert any(url.endswith("/api/v1/setup/profile") for url, _ in client.posts)
    body = client.posts[-1][1]
    assert body == {
        "profile": "ground_station",
        "ground_role": "receiver",
        "auto_restart": False,
    }
    assert not page._picker_open


@pytest.mark.asyncio
async def test_mesh_back_pops_modal() -> None:
    state = {"role": {"current": "direct", "mesh_capable": False}, "mesh": {}}
    page = MeshDetailPage()
    ctx = _ctx(state)
    await ctx.navigator.push_modal(page, ctx=ctx)
    assert ctx.navigator.modal_stack
    back = next(z for z in page.hit_zones(ctx) if z.id == "details.back")
    await page.on_touch(ctx, back, _tap(24, 24))
    assert not ctx.navigator.modal_stack


@pytest.mark.asyncio
async def test_mesh_on_leave_cancels_inflight_post() -> None:
    page = MeshDetailPage()

    async def _sleeper() -> None:
        await asyncio.sleep(60)

    page._switch_in_flight = asyncio.create_task(_sleeper())
    state = {"role": {"current": "relay"}, "mesh": {}}
    ctx = _ctx(state)
    await page.on_leave(ctx)
    assert page._switch_in_flight is None
