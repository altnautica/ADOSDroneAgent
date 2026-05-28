"""Tests for the Diagnostics drilldown detail page."""

from __future__ import annotations

from typing import Any

import pytest
import structlog
from PIL import Image

from ados.services.ui.pages import PageContext, PageNavigator
from ados.services.ui.pages.details.diagnostics import DiagnosticsDetailPage
from ados.services.ui.theme import DARK
from ados.services.ui.touch.events import TouchGesture, TouchMoveBus

_STUB_DIAG = {
    "agent": {
        "version": "0.18.2",
        "uptime_seconds": 12345,
        "process_cpu_percent": 18.4,
        "process_memory_mb": 245.0,
    },
    "board": {
        "name": "Rock 5C Lite",
        "soc": "rk3582",
        "arch": "aarch64",
        "ram_total_mb": 16384,
    },
    "system": {
        "cpu_percent": 14.2,
        "memory_used_mb": 5120,
        "memory_total_mb": 16384,
        "disk_used_gb": 24.1,
        "disk_total_gb": 117.5,
        "temp_c": 42.5,
        "load_avg": [0.5, 0.4, 0.3],
    },
    "network": {
        "ip": "192.168.1.42",
        "mac_eth0": "aa:bb:cc:dd:ee:ff",
        "mac_wlan0": "11:22:33:44:55:66",
    },
    "device": {"device_id": "ados-test-device"},
    "logs": {
        "agent": [
            "agent starting",
            "wfb pair status: paired",
            "WARNING video pipeline restarted",
            "ERROR mavlink stream stalled",
            "agent settled at 1 Hz",
            "info ground link healthy",
            "info heartbeat 1Hz",
            "info battery 12.4 V",
            "info gps fix 3, sats 11",
            "info paired drone idle",
        ],
    },
}


class _Resp:
    def __init__(self, status_code: int, payload: Any) -> None:
        self.status_code = status_code
        self._payload = payload

    def json(self) -> Any:
        return self._payload


class _StubClient:
    def __init__(self, diag: dict[str, Any] | None = None) -> None:
        self.gets: list[str] = []
        self._diag = diag or _STUB_DIAG

    async def get(self, url: str, *, timeout: float = 1.5, **_: Any) -> _Resp:
        self.gets.append(url)
        if url.endswith("/api/v1/diagnostics"):
            return _Resp(200, dict(self._diag))
        return _Resp(404, {})


class _StubDashboard:
    id = "dashboard"
    refresh_hz = 5.0

    async def on_enter(self, ctx: PageContext) -> None: ...
    async def on_leave(self, ctx: PageContext) -> None: ...
    async def render(self, ctx: PageContext) -> Image.Image:
        return Image.new("RGB", (480, 244), ctx.palette.bg_primary)
    def hit_zones(self, ctx: PageContext) -> list[Any]: return []
    async def on_touch(self, ctx: PageContext, zone: Any, g: TouchGesture) -> None: ...


def _ctx(
    *,
    http: Any | None,
    move_bus: TouchMoveBus | None = None,
) -> PageContext:
    nav = PageNavigator(registry={"dashboard": _StubDashboard()})
    return PageContext(
        state={},
        palette=DARK,
        hostname="groundnode",
        http=http,
        framebuffer=None,
        navigator=nav,
        logger=structlog.get_logger("test.diagnostics"),
        touch_move_bus=move_bus,
    )


def _drag_up(velocity: float = 800.0) -> TouchGesture:
    return TouchGesture(
        kind="drag",
        start_x=240,
        start_y=220,
        end_x=240,
        end_y=180,
        start_t_ms=0,
        end_t_ms=200,
        duration_ms=200,
        direction="up",
        velocity_px_per_s=velocity,
        samples=((240, 220, 0), (240, 180, 200)),
    )


@pytest.mark.asyncio
async def test_diagnostics_renders_all_three_sections() -> None:
    client = _StubClient()
    ctx = _ctx(http=client)
    page = DiagnosticsDetailPage()
    await page.on_enter(ctx)
    img = await page.render(ctx)
    assert img.size == (480, 244)
    # The endpoint should have been hit at least once on enter.
    assert any(g.endswith("/api/v1/diagnostics") for g in client.gets)


@pytest.mark.asyncio
async def test_diagnostics_records_metric_history_per_tick() -> None:
    client = _StubClient()
    ctx = _ctx(http=client)
    page = DiagnosticsDetailPage()
    await page.on_enter(ctx)
    # First render appends the first sample.
    await page.render(ctx)
    assert len(page._cpu_history) >= 1
    assert len(page._ram_history) >= 1
    assert len(page._temp_history) >= 1


@pytest.mark.asyncio
async def test_diagnostics_log_scroll_seeds_kinetic_decay() -> None:
    bus = TouchMoveBus()
    client = _StubClient()
    ctx = _ctx(http=client, move_bus=bus)
    page = DiagnosticsDetailPage()
    await page.on_enter(ctx)
    await page.render(ctx)
    log_zone = next(
        z for z in page.hit_zones(ctx) if z.id == "diagnostics.log_scroll"
    )
    await page.on_touch(ctx, log_zone, _drag_up())
    assert page._kinetic.active
    await page.on_leave(ctx)


@pytest.mark.asyncio
async def test_diagnostics_back_zone_pops_modal() -> None:
    client = _StubClient()
    ctx = _ctx(http=client)
    page = DiagnosticsDetailPage()
    await ctx.navigator.push_modal(page, ctx=ctx)
    assert ctx.navigator.modal_stack
    back = next(z for z in page.hit_zones(ctx) if z.id == "details.back")
    await page.on_touch(
        ctx,
        back,
        TouchGesture(
            kind="tap",
            start_x=24,
            start_y=24,
            end_x=24,
            end_y=24,
            start_t_ms=0,
            end_t_ms=10,
            duration_ms=10,
            direction=None,
            velocity_px_per_s=0.0,
            samples=((24, 24, 0),),
        ),
    )
    assert not ctx.navigator.modal_stack
