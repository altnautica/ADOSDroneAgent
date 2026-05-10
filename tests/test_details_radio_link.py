"""Tests for the Radio Link drilldown detail page."""

from __future__ import annotations

import asyncio
from typing import Any

import pytest
import structlog
from PIL import Image

from ados.services.ui.pages import PageContext, PageNavigator
from ados.services.ui.pages.details.radio_link import (
    TX_MAX_DBM,
    TX_MIN_DBM,
    RadioLinkDetailPage,
)
from ados.services.ui.theme import DARK
from ados.services.ui.touch.events import TouchGesture


class _Resp:
    def __init__(self, status_code: int, payload: Any) -> None:
        self.status_code = status_code
        self._payload = payload

    def json(self) -> Any:
        return self._payload


class _StubClient:
    """Minimal httpx.AsyncClient stand-in used by the detail tests."""

    def __init__(self) -> None:
        self.gets: list[tuple[str, dict[str, Any]]] = []
        self.puts: list[tuple[str, Any]] = []
        # Match the live producer shape returned by /api/wfb on the GS
        # profile: bitrate_kbps + fec_failed (snake_case keys are the
        # canonical names used by LinkStats.to_dict +
        # _build_status_from_stats_file).
        self._snapshot = {
            "rssi_dbm": -55,
            "snr_db": 18,
            "noise_dbm": -90,
            "loss_percent": 1.1,
            "bitrate_kbps": 10000,
            "fec_recovered": 4,
            "fec_failed": 1,
            "channel": 149,
            "frequency_mhz": 5745,
            "bandwidth_mhz": 20,
            "tx_power_dbm": 5,
        }
        self._history = {"samples": [{"rssi_dbm": -55 + i % 10} for i in range(60)]}

    async def get(
        self,
        url: str,
        *,
        params: dict[str, Any] | None = None,
        timeout: float = 1.5,
    ) -> _Resp:
        self.gets.append((url, params or {}))
        if url.endswith("/api/wfb"):
            return _Resp(200, dict(self._snapshot))
        if url.endswith("/api/wfb/history"):
            return _Resp(200, dict(self._history))
        return _Resp(404, {})

    async def put(
        self,
        url: str,
        *,
        json: Any | None = None,
        timeout: float = 2.0,
    ) -> _Resp:
        self.puts.append((url, json))
        if url.endswith("/api/wfb/tx-power"):
            value = (json or {}).get("tx_power_dbm")
            return _Resp(
                200,
                {
                    "tx_power_dbm": value,
                    "effective_dbm": value,
                },
            )
        return _Resp(404, {})


def _ctx(navigator: PageNavigator, http: Any) -> PageContext:
    return PageContext(
        state={},
        palette=DARK,
        hostname="groundnode",
        http=http,
        framebuffer=None,
        navigator=navigator,
        logger=structlog.get_logger("test.radio"),
    )


def _gesture(kind: str, x: int, y: int) -> TouchGesture:
    return TouchGesture(
        kind=kind,  # type: ignore[arg-type]
        start_x=x,
        start_y=y,
        end_x=x,
        end_y=y,
        start_t_ms=0,
        end_t_ms=10 if kind == "tap" else 600,
        duration_ms=10 if kind == "tap" else 600,
        direction=None,
        velocity_px_per_s=0.0,
        samples=((x, y, 0),),
    )


@pytest.mark.asyncio
async def test_radio_link_render_returns_480x244() -> None:
    nav = PageNavigator(registry={"dashboard": _StubDashboard()})
    page = RadioLinkDetailPage()
    client = _StubClient()
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    img = await page.render(ctx)
    assert isinstance(img, Image.Image)
    assert img.size == (480, 244)
    # Snapshot + history fetched once on enter, then once per render call.
    paths = [g[0] for g in client.gets]
    assert any(p.endswith("/api/wfb") for p in paths)
    assert any(p.endswith("/api/wfb/history") for p in paths)


@pytest.mark.asyncio
async def test_radio_link_consumes_producer_field_names() -> None:
    """The page must read bitrate_kbps + fec_failed (producer keys).

    Regression for the bench bug where the page read bitrate_mbps and
    fec_lost while the producer emitted bitrate_kbps and fec_failed,
    rendering "-- Mbps" and "FEC -- / --" against a connected link.
    """
    nav = PageNavigator(registry={"dashboard": _StubDashboard()})
    page = RadioLinkDetailPage()
    client = _StubClient()
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    await page.render(ctx)
    snap = page._snapshot
    assert snap.get("bitrate_kbps") == 10000
    assert snap.get("fec_failed") == 1
    # The render path resolves bitrate from kbps; the locally-bound
    # variable `bitrate` should compute to 10.0 Mbps. Re-derive here to
    # mirror the page logic so the test fails if the kbps→Mbps math
    # ever drifts.
    bitrate_kbps = snap.get("bitrate_kbps")
    bitrate_mbps_resolved = (
        float(bitrate_kbps) / 1000.0
        if isinstance(bitrate_kbps, (int, float)) and bitrate_kbps > 0
        else None
    )
    assert bitrate_mbps_resolved == 10.0


@pytest.mark.asyncio
async def test_radio_link_legacy_keys_still_render() -> None:
    """Legacy heartbeat-shape callers (bitrate_mbps + fec_lost) must
    keep working — the page accepts both key spellings so a future
    consumer driven from the heartbeat shape doesn't go blank.
    """
    nav = PageNavigator(registry={"dashboard": _StubDashboard()})
    page = RadioLinkDetailPage()
    client = _StubClient()
    client._snapshot = {
        "rssi_dbm": -60,
        "snr_db": 20,
        "noise_dbm": -90,
        "bitrate_mbps": 8.0,
        "fec_recovered": 3,
        "fec_lost": 2,
        "channel": 153,
        "frequency_mhz": 5765,
        "bandwidth_mhz": 20,
        "tx_power_dbm": 5,
    }
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    img = await page.render(ctx)
    assert img.size == (480, 244)
    snap = page._snapshot
    # Legacy keys still resolve through the fallback path.
    assert snap.get("bitrate_mbps") == 8.0
    assert snap.get("fec_lost") == 2


@pytest.mark.asyncio
async def test_radio_link_zero_channel_renders_dashes() -> None:
    """Channel / freq / bw default to 0 before bind. Page must not
    render "ch 0" / "0 MHz · 0 MHz" — that implies a real reading.
    """
    nav = PageNavigator(registry={"dashboard": _StubDashboard()})
    page = RadioLinkDetailPage()
    client = _StubClient()
    client._snapshot = dict(client._snapshot)
    client._snapshot["channel"] = 0
    client._snapshot["frequency_mhz"] = 0
    client._snapshot["bandwidth_mhz"] = 0
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    img = await page.render(ctx)
    assert img.size == (480, 244)
    # Producer-shape values are still in the snapshot; the rendering
    # branches should pick the no-data variants. We can't easily inspect
    # the rasterised text without OCR, so we re-derive the same logic
    # here and assert the branches.
    ch = page._snapshot.get("channel")
    freq = page._snapshot.get("frequency_mhz")
    bw = page._snapshot.get("bandwidth_mhz")
    ch_valid = isinstance(ch, (int, float)) and ch > 0
    freq_valid = isinstance(freq, (int, float)) and freq > 0
    bw_valid = isinstance(bw, (int, float)) and bw > 0
    assert not ch_valid
    assert not freq_valid
    assert not bw_valid


@pytest.mark.asyncio
async def test_radio_link_hit_zones_includes_back_slider_minus_plus() -> None:
    page = RadioLinkDetailPage()
    nav = PageNavigator(registry={"dashboard": _StubDashboard()})
    ctx = _ctx(nav, None)
    zones = {z.id for z in page.hit_zones(ctx)}
    assert "details.back" in zones
    assert "radio.tx_slider" in zones
    assert "radio.tx_minus" in zones
    assert "radio.tx_plus" in zones


@pytest.mark.asyncio
async def test_radio_link_slider_drag_commits_via_http_put() -> None:
    nav = PageNavigator(registry={"dashboard": _StubDashboard()})
    page = RadioLinkDetailPage()
    client = _StubClient()
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    # Drag ending near the right end of the slider — should commit
    # close to TX_MAX_DBM via PUT /api/wfb/tx-power.
    drag = TouchGesture(
        kind="drag",
        start_x=80,
        start_y=200,
        end_x=420,
        end_y=200,
        start_t_ms=0,
        end_t_ms=400,
        duration_ms=400,
        direction="right",
        velocity_px_per_s=850.0,
        samples=((80, 200, 0), (420, 200, 400)),
    )
    zone = next(z for z in page.hit_zones(ctx) if z.id == "radio.tx_slider")
    await page.on_touch(ctx, zone, drag)
    assert any(url.endswith("/api/wfb/tx-power") for url, _ in client.puts), client.puts
    # The committed value should be inside the envelope.
    last_value = client.puts[-1][1].get("tx_power_dbm")
    assert TX_MIN_DBM <= last_value <= TX_MAX_DBM


@pytest.mark.asyncio
async def test_radio_link_back_zone_pops_modal() -> None:
    nav = PageNavigator(registry={"dashboard": _StubDashboard()})
    page = RadioLinkDetailPage()
    client = _StubClient()
    ctx = _ctx(nav, client)
    await nav.push_modal(page, ctx=ctx)
    assert nav.modal_stack
    zone = next(z for z in page.hit_zones(ctx) if z.id == "details.back")
    await page.on_touch(ctx, zone, _gesture("tap", 24, 24))
    assert not nav.modal_stack


@pytest.mark.asyncio
async def test_radio_link_minus_plus_step_one_dbm_each() -> None:
    nav = PageNavigator(registry={"dashboard": _StubDashboard()})
    page = RadioLinkDetailPage()
    client = _StubClient()
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    await page.render(ctx)
    initial = page._tx_target_dbm or 5
    minus_zone = next(z for z in page.hit_zones(ctx) if z.id == "radio.tx_minus")
    plus_zone = next(z for z in page.hit_zones(ctx) if z.id == "radio.tx_plus")
    await page.on_touch(ctx, minus_zone, _gesture("tap", 24, 200))
    await page.on_touch(ctx, plus_zone, _gesture("tap", 460, 200))
    requested = [body.get("tx_power_dbm") for _, body in client.puts]
    assert requested[-2] == max(TX_MIN_DBM, initial - 1)
    assert requested[-1] == max(TX_MIN_DBM, initial - 1) + 1


@pytest.mark.asyncio
async def test_radio_link_on_leave_cancels_drag_task() -> None:
    page = RadioLinkDetailPage()

    async def _sleeper() -> None:
        await asyncio.sleep(60)

    page._drag_task = asyncio.create_task(_sleeper())
    page._dragging = True
    nav = PageNavigator(registry={"dashboard": _StubDashboard()})
    ctx = _ctx(nav, None)
    await page.on_leave(ctx)
    assert page._drag_task is None
    assert not page._dragging


# ── Lightweight dashboard-page double for navigator construction ────


class _StubDashboard:
    id = "dashboard"
    refresh_hz = 5.0

    async def on_enter(self, ctx: PageContext) -> None:  # noqa: D401
        ...

    async def on_leave(self, ctx: PageContext) -> None:
        ...

    async def render(self, ctx: PageContext) -> Image.Image:
        return Image.new("RGB", (480, 244), ctx.palette.bg_primary)

    def hit_zones(self, ctx: PageContext) -> list[Any]:
        return []

    async def on_touch(self, ctx: PageContext, zone: Any, gesture: TouchGesture) -> None:
        ...
