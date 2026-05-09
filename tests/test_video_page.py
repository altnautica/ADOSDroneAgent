"""Tests for the LCD Video page (render shape, recording toggle, picker)."""

from __future__ import annotations

from typing import Any

import pytest
import structlog
from PIL import Image

from ados.services.ui.pages import PageContext, PageNavigator
from ados.services.ui.pages.video import VideoPage
from ados.services.ui.theme import DARK
from ados.services.ui.touch.events import TouchGesture


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
        recording: bool = False,
        cameras: list[dict[str, Any]] | None = None,
        status_full_404: bool = False,
    ) -> None:
        self._recording = recording
        self._cameras = cameras
        self._status_full_404 = status_full_404
        self.gets: list[str] = []
        self.posts: list[tuple[str, Any]] = []

    async def get(
        self, url: str, *, timeout: float = 1.5, params: Any = None,
    ) -> _Resp:
        self.gets.append(url)
        if url.endswith("/api/wfb"):
            return _Resp(
                200,
                {
                    "channel": 149,
                    "mcs_index": 1,
                    "rssi_dbm": -55,
                    "fec_drops": 12,
                },
            )
        if url.endswith("/api/status/full"):
            if self._status_full_404:
                return _Resp(404, {})
            return _Resp(200, {"video": {"recording": self._recording}})
        if url.endswith("/api/v1/ground-station/status"):
            return _Resp(200, {"video": {"recording": self._recording}})
        if url.endswith("/api/video/cameras"):
            if self._cameras is None:
                return _Resp(404, {})
            return _Resp(200, {"cameras": self._cameras})
        if url.endswith("/v3/paths/get/main"):
            return _Resp(200, {"bytesReceived": 1_000_000})
        return _Resp(404, {})

    async def post(
        self, url: str, *, json: Any = None, timeout: float = 2.0,
    ) -> _Resp:
        self.posts.append((url, json))
        if url.endswith("/api/video/record/start"):
            self._recording = True
            return _Resp(200, {"recording": True})
        if url.endswith("/api/video/record/stop"):
            self._recording = False
            return _Resp(200, {"recording": False})
        return _Resp(200, {})


def _ctx(navigator: PageNavigator, http: Any) -> PageContext:
    return PageContext(
        state={},
        palette=DARK,
        hostname="groundnode",
        http=http,
        framebuffer=None,
        navigator=navigator,
        logger=structlog.get_logger("test.video"),
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
async def test_render_returns_480x244_when_tap_unavailable(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from ados.services.video import local_tap as lt

    async def _fail_start(self) -> None:  # type: ignore[no-untyped-def]
        raise lt.LocalVideoTapUnavailable("test stub")

    monkeypatch.setattr(lt.LocalVideoTap, "start", _fail_start)
    page = VideoPage()
    nav = PageNavigator()
    client = _StubClient()
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    img = await page.render(ctx)
    assert isinstance(img, Image.Image)
    assert img.size == (480, 244)
    await page.on_leave(ctx)


@pytest.mark.asyncio
async def test_render_paints_unavailable_card_when_gst_missing(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from ados.services.video import local_tap as lt

    async def _fail_start(self) -> None:  # type: ignore[no-untyped-def]
        raise lt.LocalVideoTapUnavailable("python3-gi missing")

    monkeypatch.setattr(lt.LocalVideoTap, "start", _fail_start)
    page = VideoPage()
    nav = PageNavigator()
    ctx = _ctx(nav, _StubClient())
    await page.on_enter(ctx)
    img = await page.render(ctx)
    # Placeholder card paints in bg_secondary.
    flat = img.getcolors(maxcolors=480 * 244) or []
    assert any(c == DARK.bg_secondary for _, c in flat)
    await page.on_leave(ctx)


@pytest.mark.asyncio
async def test_recording_button_toggles_via_post(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from ados.services.video import local_tap as lt

    async def _noop_start(self) -> None:  # type: ignore[no-untyped-def]
        raise lt.LocalVideoTapUnavailable("offscreen")

    monkeypatch.setattr(lt.LocalVideoTap, "start", _noop_start)
    page = VideoPage()
    nav = PageNavigator()
    client = _StubClient(recording=False)
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    await page.render(ctx)
    rec_zone = next(
        z for z in page.hit_zones(ctx) if z.id == "video.rec_button"
    )
    await page.on_touch(ctx, rec_zone, _tap(rec_zone.x + 4, rec_zone.y + 4))
    posted = [p for p, _ in client.posts]
    assert "/api/video/record/start" in posted
    assert page._recording is True
    await page.on_leave(ctx)


@pytest.mark.asyncio
async def test_camera_chip_hidden_with_one_camera(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from ados.services.video import local_tap as lt

    async def _noop_start(self) -> None:  # type: ignore[no-untyped-def]
        raise lt.LocalVideoTapUnavailable("offscreen")

    monkeypatch.setattr(lt.LocalVideoTap, "start", _noop_start)
    page = VideoPage()
    nav = PageNavigator()
    ctx = _ctx(nav, _StubClient())
    await page.on_enter(ctx)
    # camera_count starts at 1, camera endpoint returns 404 → still 1.
    await page._refresh_metrics_once(ctx)
    await page.render(ctx)
    zones = page.hit_zones(ctx)
    assert "video.cam_chip" not in [z.id for z in zones]


@pytest.mark.asyncio
async def test_camera_chip_visible_with_two_cameras(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from ados.services.video import local_tap as lt

    async def _noop_start(self) -> None:  # type: ignore[no-untyped-def]
        raise lt.LocalVideoTapUnavailable("offscreen")

    monkeypatch.setattr(lt.LocalVideoTap, "start", _noop_start)
    cameras = [
        {"device_path": "/dev/video0", "label": "CAM 1", "active": True},
        {"device_path": "/dev/video2", "label": "CAM 2"},
    ]
    page = VideoPage()
    nav = PageNavigator()
    client = _StubClient(cameras=cameras)
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    await page._refresh_metrics_once(ctx)
    await page.render(ctx)
    zones = page.hit_zones(ctx)
    assert "video.cam_chip" in [z.id for z in zones]
    chip_zone = next(z for z in zones if z.id == "video.cam_chip")
    await page.on_touch(
        ctx,
        chip_zone,
        _tap(chip_zone.x + 4, chip_zone.y + 4),
    )
    assert nav.modal_stack
    assert nav.modal_stack[-1].id == "modal.enum"


@pytest.mark.asyncio
async def test_surface_tap_toggles_detail_hud(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from ados.services.video import local_tap as lt

    async def _noop_start(self) -> None:  # type: ignore[no-untyped-def]
        raise lt.LocalVideoTapUnavailable("offscreen")

    monkeypatch.setattr(lt.LocalVideoTap, "start", _noop_start)
    page = VideoPage()
    nav = PageNavigator()
    ctx = _ctx(nav, _StubClient())
    await page.on_enter(ctx)
    await page.render(ctx)
    surface_zone = next(
        z for z in page.hit_zones(ctx) if z.id == "video.surface"
    )
    assert page._show_detail_hud is False
    await page.on_touch(
        ctx, surface_zone, _tap(surface_zone.x + 200, surface_zone.y + 60),
    )
    assert page._show_detail_hud is True
    await page.on_touch(
        ctx, surface_zone, _tap(surface_zone.x + 200, surface_zone.y + 60),
    )
    assert page._show_detail_hud is False


@pytest.mark.asyncio
async def test_status_full_404_falls_back_to_ground_station_status(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from ados.services.video import local_tap as lt

    async def _noop_start(self) -> None:  # type: ignore[no-untyped-def]
        raise lt.LocalVideoTapUnavailable("offscreen")

    monkeypatch.setattr(lt.LocalVideoTap, "start", _noop_start)
    page = VideoPage()
    nav = PageNavigator()
    client = _StubClient(recording=True, status_full_404=True)
    ctx = _ctx(nav, client)
    await page.on_enter(ctx)
    await page._refresh_metrics_once(ctx)
    assert page._recording is True
    assert any(g.endswith("/api/v1/ground-station/status") for g in client.gets)


@pytest.mark.asyncio
async def test_unavailable_tap_does_not_retry_within_cooldown(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A failed tap.start() must not be retried until the cooldown
    window elapses. Earlier code cached the failure forever — this
    test guards against regression in the OTHER direction (retrying
    on every render tick, which would re-spin gstreamer at ~1 Hz)."""
    from ados.services.video import local_tap as lt
    from ados.services.ui.pages import video as video_mod

    attempts: list[int] = []

    async def _fail_start(self) -> None:  # type: ignore[no-untyped-def]
        attempts.append(1)
        raise lt.LocalVideoTapUnavailable("stub failure")

    monkeypatch.setattr(lt.LocalVideoTap, "start", _fail_start)
    # Freeze monotonic so the cooldown gate stays closed for both calls.
    monkeypatch.setattr(video_mod.time, "monotonic", lambda: 100.0)

    page = VideoPage()
    nav = PageNavigator()
    ctx = _ctx(nav, _StubClient())

    await page._ensure_tap(ctx)
    await page._ensure_tap(ctx)

    assert len(attempts) == 1
    assert page._tap_unavailable_reason == "stub failure"
    assert page._tap_unavailable_at == 100.0


@pytest.mark.asyncio
async def test_unavailable_tap_retries_after_cooldown(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """After the cooldown window elapses, the page must clear the
    cached failure and re-attempt tap.start() so the LCD recovers
    once mediamtx-gs and the ffmpeg sidecar finish coming up."""
    from ados.services.video import local_tap as lt
    from ados.services.ui.pages import video as video_mod

    attempts: list[float] = []

    # First call fails, second call succeeds.
    call_idx = {"n": 0}

    async def _flaky_start(self) -> None:  # type: ignore[no-untyped-def]
        call_idx["n"] += 1
        attempts.append(time_ref["t"])
        if call_idx["n"] == 1:
            raise lt.LocalVideoTapUnavailable("first attempt fails")
        # Second call succeeds — no exception.

    time_ref = {"t": 100.0}
    monkeypatch.setattr(lt.LocalVideoTap, "start", _flaky_start)
    monkeypatch.setattr(video_mod.time, "monotonic", lambda: time_ref["t"])

    page = VideoPage()
    nav = PageNavigator()
    ctx = _ctx(nav, _StubClient())

    # First attempt: fails, gets cached.
    await page._ensure_tap(ctx)
    assert page._tap_unavailable_reason is not None
    assert page._tap is None

    # Advance just shy of the cooldown — still gated.
    time_ref["t"] = 100.0 + video_mod._TAP_RETRY_COOLDOWN_SECONDS - 0.1
    await page._ensure_tap(ctx)
    assert len(attempts) == 1, "should still be in cooldown"

    # Advance past the cooldown — retry fires and succeeds.
    time_ref["t"] = 100.0 + video_mod._TAP_RETRY_COOLDOWN_SECONDS + 0.1
    await page._ensure_tap(ctx)
    assert len(attempts) == 2
    assert page._tap_unavailable_reason is None
    assert page._tap_unavailable_at is None
    assert page._tap is not None


@pytest.mark.asyncio
async def test_first_attempt_records_failure_timestamp(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The cached failure must come with a monotonic timestamp so the
    cooldown gate has something to compare against."""
    from ados.services.video import local_tap as lt
    from ados.services.ui.pages import video as video_mod

    async def _fail_start(self) -> None:  # type: ignore[no-untyped-def]
        raise lt.LocalVideoTapUnavailable("stub")

    monkeypatch.setattr(lt.LocalVideoTap, "start", _fail_start)
    monkeypatch.setattr(video_mod.time, "monotonic", lambda: 555.5)

    page = VideoPage()
    nav = PageNavigator()
    await page._ensure_tap(_ctx(nav, _StubClient()))

    assert page._tap_unavailable_reason == "stub"
    assert page._tap_unavailable_at == 555.5


@pytest.mark.asyncio
async def test_render_drives_retry_when_user_sits_on_failed_page(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """When the operator sits on the Video tab past the cooldown after a
    failed tap, render() must invoke _ensure_tap so the live frame can
    appear without the operator having to navigate away and back.

    Regression for the gap between v0.19.15 (cooldown logic) and
    v0.19.19 (render-loop hook): the cooldown was correct but only
    on_enter ever called _ensure_tap, so a tap that failed at first
    open stayed unavailable forever even after RTSP came up.
    """
    from ados.services.video import local_tap as lt
    from ados.services.ui.pages import video as video_mod

    call_idx = {"n": 0}

    async def _flaky_start(self) -> None:  # type: ignore[no-untyped-def]
        call_idx["n"] += 1
        if call_idx["n"] == 1:
            raise lt.LocalVideoTapUnavailable("first attempt fails")
        # Second call (from render) succeeds.

    time_ref = {"t": 100.0}
    monkeypatch.setattr(lt.LocalVideoTap, "start", _flaky_start)
    monkeypatch.setattr(video_mod.time, "monotonic", lambda: time_ref["t"])

    page = VideoPage()
    nav = PageNavigator()
    ctx = _ctx(nav, _StubClient())

    # Simulate operator opening Video tab; first attempt fails.
    await page.on_enter(ctx)
    assert page._tap_unavailable_reason == "first attempt fails"
    assert page._tap is None
    assert call_idx["n"] == 1

    # Operator sits on the page. render() ticks within the cooldown
    # window — must NOT spin gstreamer.
    time_ref["t"] = 100.0 + 5.0  # 5 s in
    await page.render(ctx)
    assert call_idx["n"] == 1, "render must respect cooldown"

    # Cooldown elapses. Next render() must drive the retry which
    # succeeds and clears the cached failure.
    time_ref["t"] = 100.0 + video_mod._TAP_RETRY_COOLDOWN_SECONDS + 0.1
    await page.render(ctx)
    assert call_idx["n"] == 2
    assert page._tap_unavailable_reason is None
    assert page._tap is not None


@pytest.mark.asyncio
async def test_render_skips_retry_when_tap_already_alive(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The render-side retry hook must short-circuit when the tap is
    already alive so we don't pay attribute reads on the happy path."""
    from ados.services.video import local_tap as lt
    from ados.services.ui.pages import video as video_mod

    start_calls = {"n": 0}

    async def _ok_start(self) -> None:  # type: ignore[no-untyped-def]
        start_calls["n"] += 1

    monkeypatch.setattr(lt.LocalVideoTap, "start", _ok_start)
    monkeypatch.setattr(video_mod.time, "monotonic", lambda: 1.0)

    page = VideoPage()
    nav = PageNavigator()
    ctx = _ctx(nav, _StubClient())

    await page.on_enter(ctx)
    assert page._tap is not None
    assert start_calls["n"] == 1

    # Two render ticks: should not call start again because the tap
    # is alive (the render hook only fires when _tap is None and a
    # cached failure is present).
    await page.render(ctx)
    await page.render(ctx)
    assert start_calls["n"] == 1
