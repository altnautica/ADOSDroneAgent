"""Tests for the Settings page (drag-scroll, row dispatch, banner)."""

from __future__ import annotations

from typing import Any

import pytest
import structlog
from PIL import Image

from ados.services.ui.pages import PageContext, PageNavigator
from ados.services.ui.pages.settings import SettingsPage
from ados.services.ui.theme import DARK
from ados.services.ui.touch.events import TouchGesture


class _Resp:
    def __init__(self, status_code: int, payload: Any) -> None:
        self.status_code = status_code
        self._payload = payload

    def json(self) -> Any:
        return self._payload


class _StubClient:
    """Captures every REST call so the tests can assert dispatch."""

    def __init__(self) -> None:
        self.gets: list[tuple[str, dict[str, Any]]] = []
        self.posts: list[tuple[str, Any]] = []
        self.puts: list[tuple[str, Any]] = []
        # Static snapshots returned to the page on refresh.
        self._wfb = {
            "channel": 149,
            "tx_power_dbm": 5,
            "mcs_index": 1,
            "topology": "host_vbus",
            "auto_pair_enabled": True,
        }
        self._gs_status = {"role": {"current": "receiver"}}
        self._setup_status = {
            "version": "0.17.3",
            "device_id": "test-device",
            "device_name": "groundnode",
            "network": {
                "hostname": "groundnode",
                "hotspot_enabled": True,
                "hotspot_ssid": "ADOS-AP",
            },
            "cloud_choice": {"mode": "cloud"},
        }

    async def get(self, url: str, *, timeout: float = 1.5, params: Any = None) -> _Resp:
        self.gets.append((url, params or {}))
        if url.endswith("/api/wfb"):
            return _Resp(200, dict(self._wfb))
        if url.endswith("/api/v1/ground-station/status"):
            return _Resp(200, dict(self._gs_status))
        if url.endswith("/api/v1/setup/status"):
            return _Resp(200, dict(self._setup_status))
        return _Resp(404, {})

    async def post(self, url: str, *, json: Any = None, timeout: float = 2.0) -> _Resp:
        self.posts.append((url, json))
        return _Resp(200, {"ok": True, "overall": True})

    async def put(self, url: str, *, json: Any = None, timeout: float = 2.0) -> _Resp:
        self.puts.append((url, json))
        return _Resp(200, {"tx_power_dbm": (json or {}).get("tx_power_dbm")})


def _state() -> dict[str, Any]:
    return {
        "wfb": {
            "channel": 149,
            "tx_power_dbm": 5,
            "mcs_index": 1,
            "topology": "host_vbus",
            "auto_pair_enabled": True,
        },
        "network": {
            "hotspot": {"ssid": "ADOS-AP", "enabled": True},
            "wifi_client": {"enabled": False},
        },
        "role": {"current": "receiver"},
        "server": {"mode": "cloud"},
        "logging": {"level": "info"},
        "ui": {"theme": "dark"},
        "pending_reboot_count": 0,
    }


def _ctx(navigator: PageNavigator, http: Any, *, state: dict | None = None) -> PageContext:
    return PageContext(
        state=state if state is not None else _state(),
        palette=DARK,
        hostname="groundnode",
        http=http,
        framebuffer=None,
        navigator=navigator,
        logger=structlog.get_logger("test.settings"),
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


def _drag_up(velocity: float = 600.0) -> TouchGesture:
    return TouchGesture(
        kind="drag",
        start_x=240,
        start_y=200,
        end_x=240,
        end_y=80,
        start_t_ms=0,
        end_t_ms=400,
        duration_ms=400,
        direction="up",
        velocity_px_per_s=velocity,
        samples=((240, 200, 0), (240, 80, 400)),
    )


@pytest.mark.asyncio
async def test_render_returns_480x244() -> None:
    page = SettingsPage()
    nav = PageNavigator()
    client = _StubClient()
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    img = await page.render(ctx)
    assert isinstance(img, Image.Image)
    assert img.size == (480, 244)


@pytest.mark.asyncio
async def test_drag_seeds_kinetic_decay() -> None:
    page = SettingsPage()
    nav = PageNavigator()
    client = _StubClient()
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    # Render once to populate visible zones.
    await page.render(ctx)
    zones = page.hit_zones(ctx)
    row_zone = next(z for z in zones if z.id.startswith("row:"))
    await page.on_touch(ctx, row_zone, _drag_up())
    assert page._kinetic.active


@pytest.mark.asyncio
async def test_kinetic_tick_advances_offset() -> None:
    page = SettingsPage()
    nav = PageNavigator()
    client = _StubClient()
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    page._kinetic.start(800.0)
    initial = page._y_offset
    await page.render(ctx)
    # Repeat render to allow time to elapse.
    await page.render(ctx)
    # The offset should have advanced (positive velocity scrolls list up
    # → offset increases).
    assert page._y_offset != initial


@pytest.mark.asyncio
async def test_tap_channel_row_pushes_enum_modal() -> None:
    page = SettingsPage()
    nav = PageNavigator()
    client = _StubClient()
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    await page.render(ctx)
    zone = next(z for z in page.hit_zones(ctx) if z.id == "row:wfb.channel")
    await page.on_touch(ctx, zone, _tap(zone.x + 10, zone.y + 10))
    assert nav.modal_stack
    assert nav.modal_stack[-1].id == "modal.enum"


@pytest.mark.asyncio
async def test_tap_tx_power_row_pushes_slider_modal() -> None:
    page = SettingsPage()
    nav = PageNavigator()
    client = _StubClient()
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    await page.render(ctx)
    zone = next(z for z in page.hit_zones(ctx) if z.id == "row:wfb.tx_power_dbm")
    await page.on_touch(ctx, zone, _tap(zone.x + 10, zone.y + 10))
    assert nav.modal_stack
    assert nav.modal_stack[-1].id == "modal.slider"


@pytest.mark.asyncio
async def test_tap_hotspot_toggle_off_pushes_confirm_dialog() -> None:
    """Disabling the hotspot is destructive → confirm dialog first."""
    page = SettingsPage()
    nav = PageNavigator()
    client = _StubClient()
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    await page.render(ctx)
    zone = next(z for z in page.hit_zones(ctx) if z.id == "row:network.hotspot.on")
    await page.on_touch(ctx, zone, _tap(zone.x + 10, zone.y + 10))
    assert nav.modal_stack
    assert nav.modal_stack[-1].id == "modal.confirm"


@pytest.mark.asyncio
async def test_reboot_banner_renders_when_pending_count_positive() -> None:
    page = SettingsPage()
    nav = PageNavigator()
    client = _StubClient()
    state = _state()
    state["pending_reboot_count"] = 3
    ctx = _ctx(nav, client, state=state)
    await page.on_enter(ctx)
    img = await page.render(ctx)
    flat = img.getcolors(maxcolors=480 * 244) or []
    assert any(c == DARK.status_warning for _, c in flat)
    # The banner zone should be in the hit-zone list now.
    ids = {z.id for z in page.hit_zones(ctx)}
    assert "banner.reboot" in ids


@pytest.mark.asyncio
async def test_reboot_banner_tap_pushes_confirm_dialog() -> None:
    page = SettingsPage()
    nav = PageNavigator()
    client = _StubClient()
    state = _state()
    state["pending_reboot_count"] = 1
    ctx = _ctx(nav, client, state=state)
    await page.on_enter(ctx)
    await page.render(ctx)
    zone = next(z for z in page.hit_zones(ctx) if z.id == "banner.reboot")
    await page.on_touch(ctx, zone, _tap(zone.x + 100, zone.y + 10))
    assert nav.modal_stack
    assert nav.modal_stack[-1].id == "modal.confirm"


@pytest.mark.asyncio
async def test_pending_reboot_bumps_when_channel_changes() -> None:
    """Walk a full enum → save flow and confirm pending counter increments."""
    page = SettingsPage()
    nav = PageNavigator()
    client = _StubClient()
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    await page.render(ctx)
    zone = next(z for z in page.hit_zones(ctx) if z.id == "row:wfb.channel")
    await page.on_touch(ctx, zone, _tap(zone.x + 10, zone.y + 10))
    enum_modal = nav.modal_stack[-1]
    enum_zones = enum_modal.hit_zones(ctx)
    # Pick option 153 which differs from the cached snapshot value of 149.
    chosen = next(z for z in enum_zones if z.id == "enum.option:153")
    await enum_modal.on_touch(ctx, chosen, _tap(chosen.x + 10, chosen.y + 10))
    # Channel commit → POST /api/wfb/channel + bump pending reboot.
    posted_paths = [p for p, _ in client.posts]
    assert any(p.endswith("/api/wfb/channel") for p in posted_paths)
    assert ctx.state.get("pending_reboot_count", 0) == 1
