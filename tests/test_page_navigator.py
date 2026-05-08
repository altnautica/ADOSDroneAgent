"""Tests for the LCD page navigator and modal stack."""

from __future__ import annotations

import asyncio
import json
from pathlib import Path
from typing import ClassVar

import pytest
import structlog
from PIL import Image

from ados.services.ui.pages import HitZone, Page, PageContext, PageNavigator
from ados.services.ui.pages.dashboard import DashboardPage
from ados.services.ui.theme import DARK
from ados.services.ui.touch.events import TouchGesture


class _SpyPage:
    """Page double that records every lifecycle call."""

    id: ClassVar[str] = "spy"
    refresh_hz: ClassVar[float] = 5.0

    def __init__(self, page_id: str = "spy") -> None:
        self.id = page_id  # type: ignore[misc]
        self.events: list[str] = []

    async def on_enter(self, ctx: PageContext) -> None:
        self.events.append("enter")

    async def on_leave(self, ctx: PageContext) -> None:
        self.events.append("leave")

    async def render(self, ctx: PageContext) -> Image.Image:
        return Image.new("RGB", (480, 244), ctx.palette.bg_primary)

    def hit_zones(self, ctx: PageContext) -> list[HitZone]:
        return [HitZone(id="spy.tile", x=0, y=0, w=240, h=122)]

    async def on_touch(
        self, ctx: PageContext, zone: HitZone, gesture: TouchGesture,
    ) -> None:
        self.events.append(f"touch:{zone.id}")


def _make_ctx(navigator: PageNavigator) -> PageContext:
    return PageContext(
        state={},
        palette=DARK,
        hostname="groundnode",
        http=None,
        framebuffer=None,
        navigator=navigator,
        logger=structlog.get_logger("test"),
    )


@pytest.mark.asyncio
async def test_go_calls_on_leave_then_on_enter(tmp_path: Path):
    nav = PageNavigator(state_path=tmp_path / "lcd-state.json")
    a = _SpyPage("a")
    b = _SpyPage("b")
    nav.register(a)
    nav.register(b)
    nav.active_page_id = "a"
    ctx = _make_ctx(nav)

    changed = await nav.go("b", ctx=ctx)
    assert changed is True
    assert a.events == ["leave"]
    assert b.events == ["enter"]
    assert nav.active_page_id == "b"


@pytest.mark.asyncio
async def test_go_to_same_id_is_noop(tmp_path: Path):
    nav = PageNavigator(state_path=tmp_path / "lcd-state.json")
    a = _SpyPage("a")
    nav.register(a)
    nav.active_page_id = "a"
    ctx = _make_ctx(nav)
    changed = await nav.go("a", ctx=ctx)
    assert changed is False
    assert a.events == []


@pytest.mark.asyncio
async def test_modal_push_pop_lifecycle(tmp_path: Path):
    nav = PageNavigator(state_path=tmp_path / "lcd-state.json")
    base = _SpyPage("base")
    modal = _SpyPage("modal")
    nav.register(base)
    nav.active_page_id = "base"
    ctx = _make_ctx(nav)

    await nav.push_modal(modal, ctx=ctx)
    assert nav.modal_stack[-1] is modal
    assert nav.current_page() is modal
    assert modal.events == ["enter"]
    popped = await nav.pop_modal(ctx=ctx)
    assert popped is modal
    assert nav.modal_stack == []
    assert nav.current_page() is base
    assert modal.events == ["enter", "leave"]


@pytest.mark.asyncio
async def test_persists_active_id(tmp_path: Path):
    state_path = tmp_path / "lcd-state.json"
    nav = PageNavigator(state_path=state_path)
    nav.register(_SpyPage("a"))
    nav.register(_SpyPage("b"))
    ctx = _make_ctx(nav)
    await nav.go("b", ctx=ctx)
    assert state_path.exists()
    blob = json.loads(state_path.read_text())
    assert blob["active_page_id"] == "b"


@pytest.mark.asyncio
async def test_loads_persisted_id_on_construct(tmp_path: Path):
    state_path = tmp_path / "lcd-state.json"
    state_path.write_text('{"active_page_id":"video","modal_stack":[]}')
    nav = PageNavigator(state_path=state_path)
    nav.register(_SpyPage("dashboard"))
    nav.register(_SpyPage("video"))
    # Re-init after registration: active should be "video" (persisted).
    nav2 = PageNavigator(
        registry={"dashboard": _SpyPage("dashboard"), "video": _SpyPage("video")},
        state_path=state_path,
    )
    assert nav2.active_page_id == "video"


@pytest.mark.asyncio
async def test_dashboard_page_renders_end_to_end(tmp_path: Path):
    nav = PageNavigator(state_path=tmp_path / "lcd-state.json")
    page = DashboardPage()
    nav.register(page)
    nav.active_page_id = "dashboard"
    ctx = _make_ctx(nav)
    img = await page.render(ctx)
    assert img.size == (480, 244)
    zones = page.hit_zones(ctx)
    assert len(zones) == 4
    # Tile zone ids match the dashboard drilldown contract.
    assert {z.id for z in zones} == {
        "tile.radio_link",
        "tile.drone",
        "tile.mesh",
        "tile.uplink",
    }


@pytest.mark.asyncio
async def test_record_tap_surfaces_in_feedback(tmp_path: Path):
    nav = PageNavigator(state_path=tmp_path / "lcd-state.json")
    nav.record_tap("tab.dashboard", 1234)
    fb = nav.tap_feedback()
    assert fb["tab.dashboard"] == 1234


@pytest.mark.asyncio
async def test_go_to_unknown_page_returns_false(tmp_path: Path):
    nav = PageNavigator(state_path=tmp_path / "lcd-state.json")
    ctx = _make_ctx(nav)
    nav.register(_SpyPage("dashboard"))
    nav.active_page_id = "dashboard"
    changed = await nav.go("nonexistent", ctx=ctx)
    assert changed is False
    assert nav.active_page_id == "dashboard"


def test_hit_zone_contains_inclusive_bounds():
    zone = HitZone(id="z", x=10, y=20, w=100, h=50)
    assert zone.contains(10, 20) is True
    assert zone.contains(109, 69) is True
    assert zone.contains(110, 70) is False
    assert zone.contains(9, 20) is False
