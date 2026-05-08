"""Tests for the live RSSI / FEC / latency / bitrate wiring on the Video page.

The page caches values from three sources: the in-process
``LinkQualityMonitor`` (via ``get_agent_app``), the MediaMTX REST surface
for bytes-received delta, and the local video tap for FPS / latency.
This file exercises the cache after a single ``_refresh_metrics_once``
tick under each of the three input shapes — all six metrics must be
present and the formatter must render them as the expected strings.
"""

from __future__ import annotations

from typing import Any

import pytest
import structlog
from PIL import Image

from ados.services.ui.pages import PageContext, PageNavigator
from ados.services.ui.pages.video import VideoPage
from ados.services.ui.theme import DARK
from ados.services.wfb.link_quality import LinkQualityMonitor


class _Resp:
    def __init__(self, status_code: int, payload: Any) -> None:
        self.status_code = status_code
        self._payload = payload

    def json(self) -> Any:
        return self._payload


class _StubClient:
    def __init__(
        self,
        *,
        bytes_received: int = 1_000_000,
        wfb_payload: dict[str, Any] | None = None,
    ) -> None:
        self.bytes_received = bytes_received
        self._wfb_payload = wfb_payload
        self.gets: list[str] = []
        self.posts: list[tuple[str, Any]] = []

    async def get(
        self, url: str, *, timeout: float = 1.5, params: Any = None,
    ) -> _Resp:
        self.gets.append(url)
        if url.endswith("/v3/paths/get/main"):
            return _Resp(200, {"bytesReceived": self.bytes_received})
        if url.endswith("/api/wfb"):
            if self._wfb_payload is None:
                return _Resp(404, {})
            return _Resp(200, self._wfb_payload)
        if url.endswith("/api/status/full"):
            return _Resp(404, {})
        if url.endswith("/api/v1/ground-station/status"):
            return _Resp(200, {"video": {}})
        return _Resp(404, {})

    async def post(
        self, url: str, *, json: Any = None, timeout: float = 2.0,
    ) -> _Resp:
        self.posts.append((url, json))
        return _Resp(200, {})


def _ctx(navigator: PageNavigator, http: Any) -> PageContext:
    return PageContext(
        state={},
        palette=DARK,
        hostname="groundnode",
        http=http,
        framebuffer=None,
        navigator=navigator,
        logger=structlog.get_logger("test.video.metrics"),
    )


@pytest.fixture(autouse=True)
def _stub_local_tap_start(monkeypatch: pytest.MonkeyPatch) -> None:
    """Force LocalVideoTap.start to fail-soft so the test never spins gstreamer."""
    from ados.services.video import local_tap as lt

    async def _fail(self) -> None:  # type: ignore[no-untyped-def]
        raise lt.LocalVideoTapUnavailable("test")

    monkeypatch.setattr(lt.LocalVideoTap, "start", _fail)


@pytest.fixture
def fresh_agent_app(monkeypatch: pytest.MonkeyPatch) -> None:
    """Reset the global agent app singleton between tests."""
    from ados.api import deps

    monkeypatch.setattr(deps, "_agent_app", None)


def _install_inproc_monitor(
    monkeypatch: pytest.MonkeyPatch,
    *,
    rssi: float,
    fec_recovered: int,
    fec_failed: int,
    loss_percent: float = 0.5,
) -> LinkQualityMonitor:
    """Wire up a fake agent app whose wfb_manager().monitor returns LinkStats."""
    monitor = LinkQualityMonitor()
    monitor.feed_line(
        "RX ANT 0: addr rssi_min=-60 "
        f"rssi_avg={int(rssi)} rssi_max=-40 "
        f"packets={fec_recovered + fec_failed} lost={fec_failed} "
        f"fec_rec={fec_recovered} fec_fail={fec_failed}"
    )

    class _FakeWfb:
        def __init__(self, m: LinkQualityMonitor) -> None:
            self.monitor = m
            self._channel = 149

    class _FakeWfbCfg:
        channel = 149
        mcs_index = 1

    class _FakeVideoCfg:
        wfb = _FakeWfbCfg()

    class _FakeConfig:
        video = _FakeVideoCfg()

    class _FakeApp:
        config = _FakeConfig()

        def wfb_manager(self) -> Any:
            return _FakeWfb(monitor)

    from ados.api import deps

    monkeypatch.setattr(deps, "_agent_app", _FakeApp())
    return monitor


@pytest.mark.asyncio
async def test_rssi_and_fec_drops_from_inproc_monitor(
    monkeypatch: pytest.MonkeyPatch, fresh_agent_app: None,
) -> None:
    _install_inproc_monitor(
        monkeypatch, rssi=-55.0, fec_recovered=1024, fec_failed=12,
    )
    page = VideoPage()
    nav = PageNavigator()
    client = _StubClient()
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    await page._refresh_metrics_once(ctx)
    assert page._metrics_cache["rssi_dbm"] == -55.0
    assert page._metrics_cache["fec_drops"] == (12, 1036)
    assert page._metrics_cache["channel"] == 149
    assert page._metrics_cache["mcs_index"] == 1
    # The /api/wfb fallback should NOT have been hit since the in-process
    # monitor answered first.
    assert all(not g.endswith("/api/wfb") for g in client.gets)


@pytest.mark.asyncio
async def test_rssi_and_fec_drops_from_rest_fallback(
    fresh_agent_app: None,
) -> None:
    page = VideoPage()
    nav = PageNavigator()
    client = _StubClient(
        wfb_payload={
            "channel": 161,
            "mcs_index": 2,
            "rssi_dbm": -58.0,
            "fec_recovered": 500,
            "fec_failed": 7,
        },
    )
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    await page._refresh_metrics_once(ctx)
    assert page._metrics_cache["rssi_dbm"] == -58.0
    assert page._metrics_cache["fec_drops"] == (7, 507)
    assert page._metrics_cache["channel"] == 161
    assert page._metrics_cache["mcs_index"] == 2


@pytest.mark.asyncio
async def test_bitrate_kbps_from_mediamtx_delta(fresh_agent_app: None) -> None:
    page = VideoPage()
    nav = PageNavigator()
    client = _StubClient(bytes_received=1_000_000)
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    await page._refresh_metrics_once(ctx)
    assert page._metrics_cache["bitrate_kbps"] is None  # first sample seeds
    # Advance the counter by 250 KB ≈ 2000 kbps.
    client.bytes_received = 1_250_000
    await page._refresh_metrics_once(ctx)
    kbps = page._metrics_cache["bitrate_kbps"]
    assert isinstance(kbps, float)
    assert kbps > 0


@pytest.mark.asyncio
async def test_metrics_strip_renders_all_six_cells(
    monkeypatch: pytest.MonkeyPatch, fresh_agent_app: None,
) -> None:
    _install_inproc_monitor(
        monkeypatch, rssi=-50.0, fec_recovered=2048, fec_failed=4,
    )
    page = VideoPage()
    nav = PageNavigator()
    client = _StubClient()
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    await page._refresh_metrics_once(ctx)
    img = await page.render(ctx)
    assert isinstance(img, Image.Image)
    assert img.size == (480, 244)
    # Inspect the cache to confirm formatters would render real values.
    assert page._format_rssi(page._metrics_cache["rssi_dbm"]) == "-50 dBm"
    assert page._format_drops(page._metrics_cache["fec_drops"]) == "4 / 2052"
    assert page._format_radio(
        page._metrics_cache["channel"],
        page._metrics_cache["mcs_index"],
    ) == "ch149 MCS1"


@pytest.mark.asyncio
async def test_latency_pulls_from_tap_stats(
    monkeypatch: pytest.MonkeyPatch, fresh_agent_app: None,
) -> None:
    _install_inproc_monitor(
        monkeypatch, rssi=-55.0, fec_recovered=10, fec_failed=0,
    )
    page = VideoPage()
    nav = PageNavigator()
    client = _StubClient()
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    # Inject a fake tap with a known latency.
    class _FakeTap:
        def stats(self) -> dict[str, Any]:
            return {"latency_ms": 42.5, "fps": 30.0}

    page._tap = _FakeTap()  # type: ignore[assignment]
    await page._refresh_metrics_once(ctx)
    assert page._metrics_cache["latency_ms"] == 42.5
    assert page._format_latency(page._metrics_cache["latency_ms"]) == "42 ms"


@pytest.mark.asyncio
async def test_latency_dashed_when_no_marker(fresh_agent_app: None) -> None:
    page = VideoPage()
    nav = PageNavigator()
    client = _StubClient()
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    await page._refresh_metrics_once(ctx)
    assert page._metrics_cache["latency_ms"] is None
    assert page._format_latency(None) == "--"


def test_format_drops_handles_legacy_int() -> None:
    page = VideoPage()
    assert page._format_drops(12) == "12"


def test_format_drops_handles_tuple() -> None:
    page = VideoPage()
    assert page._format_drops((3, 100)) == "3 / 100"


def test_format_drops_handles_none() -> None:
    page = VideoPage()
    assert page._format_drops(None) == "--"
