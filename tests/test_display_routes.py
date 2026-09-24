"""Tests for the ``/api/v1/display/*`` REST surface.

Covers:

* ``POST /calibrate/start`` queues the native wizard through the recalibrate
  flag; ``GET /calibrate/status`` reports the on-disk fit and a pending request.
* ``GET /snapshot`` serves the native frame, falling back to the framebuffer.
* ``GET /page`` reads the navigator's persisted JSON; ``POST /page`` validates
  the page id and writes the request file.
"""

from __future__ import annotations

import json
from pathlib import Path
from unittest.mock import patch

import pytest
from fastapi.testclient import TestClient

from ados.api.routes import display as display_routes
from ados.api.server import create_app
from tests.api_runtime_utils import build_api_runtime


@pytest.fixture
def client() -> TestClient:
    return TestClient(create_app(build_api_runtime()))


@pytest.fixture
def calib_paths(tmp_path: Path):
    flag = tmp_path / "recalibrate.flag"
    calib = tmp_path / "touch.calib"
    with patch.object(display_routes, "RECALIBRATE_FLAG_PATH", flag), \
         patch.object(display_routes, "TOUCH_CALIB_PATH", calib):
        yield flag, calib


# ── calibrate -------------------------------------------------------


def test_calibrate_start_queues_the_native_wizard(client: TestClient, calib_paths) -> None:
    flag, _calib = calib_paths
    resp = client.post("/api/v1/display/calibrate/start")
    assert resp.status_code == 200
    assert resp.json() == {"requested": True, "target_count": 9}
    # The native display service launches its wizard when this file appears.
    assert flag.read_text() == "1\n"
    status = client.get("/api/v1/display/calibrate/status").json()
    assert status == {"calibrated": False, "requested": True}


def test_calibrate_status_reads_the_fit_on_disk(client: TestClient, calib_paths) -> None:
    _flag, calib = calib_paths
    with patch.object(display_routes, "load_calib", return_value=object()):
        status = client.get("/api/v1/display/calibrate/status").json()
    assert status == {"calibrated": True, "requested": False}


def test_setup_calibrate_start_uses_the_same_request(client: TestClient, calib_paths) -> None:
    flag, _calib = calib_paths
    body = client.post("/api/v1/setup/display/calibrate/start").json()
    assert body["ok"] is True
    assert "restart" not in body["message"].lower()
    assert flag.exists()


# ── snapshot --------------------------------------------------------


def test_snapshot_returns_404_when_nothing_to_show(client: TestClient) -> None:
    """No native frame and no readable framebuffer: 404 with the canonical detail."""
    display_routes._snap_cache.clear()
    with patch.object(display_routes, "_render_snapshot_png", return_value=None):
        resp = client.get("/api/v1/display/snapshot")
    assert resp.status_code == 404
    assert resp.json()["detail"] == "no_lcd_bound"


def test_snapshot_serves_a_fresh_native_frame_without_an_spi_framebuffer(
    client: TestClient, tmp_path: Path,
) -> None:
    """A panel the native writer drives (DRM, HDMI) has no fbtft framebuffer;
    its fresh frame must still reach the GCS preview."""
    snap = tmp_path / "lcd-snapshot.png"
    snap.write_bytes(b"\x89PNG\r\n\x1a\nnative-frame")
    display_routes._snap_cache.clear()
    with patch.object(display_routes, "LCD_SNAPSHOT_PATH", snap), \
         patch.object(display_routes, "_resolve_fb_path", return_value=None):
        resp = client.get("/api/v1/display/snapshot")
    assert resp.status_code == 200
    assert resp.content == b"\x89PNG\r\n\x1a\nnative-frame"


def test_snapshot_returns_png_when_lcd_present(client: TestClient) -> None:
    """When _render_snapshot_png returns bytes, the response is image/png."""
    display_routes._snap_cache.clear()
    fake_png = b"\x89PNG\r\n\x1a\nfake-bytes"
    with patch.object(display_routes, "_render_snapshot_png", return_value=fake_png):
        resp = client.get("/api/v1/display/snapshot?width=240&height=160")
    assert resp.status_code == 200
    assert resp.headers["content-type"] == "image/png"
    assert resp.content == fake_png


def test_snapshot_caches_repeated_requests(client: TestClient) -> None:
    """Two snapshots within the cache TTL produce a single render call."""
    # Reset the cache so this test starts clean.
    display_routes._snap_cache.clear()
    fake_png = b"\x89PNG\r\n\x1a\nfake-bytes"
    with patch.object(
        display_routes, "_render_snapshot_png", return_value=fake_png,
    ) as mock_render:
        client.get("/api/v1/display/snapshot?width=240&height=160")
        client.get("/api/v1/display/snapshot?width=240&height=160")
    # Cache holds the first result; the second request reuses it.
    assert mock_render.call_count == 1


def test_render_prefers_fresh_rust_snapshot(tmp_path: Path) -> None:
    """When the native writer's PNG is fresh, it is served verbatim and the
    framebuffer is never read."""
    snap = tmp_path / "lcd-snapshot.png"
    rust_png = b"\x89PNG\r\n\x1a\nrust-frame"
    snap.write_bytes(rust_png)
    display_routes._snap_cache.clear()
    with patch.object(display_routes, "LCD_SNAPSHOT_PATH", snap), \
         patch.object(
             display_routes, "render_framebuffer_png",
         ) as mock_fb:
        out = display_routes._render_snapshot_png(240, 160)
    assert out == rust_png
    # The framebuffer fallback must not run when the sidecar is fresh.
    mock_fb.assert_not_called()


def test_render_falls_back_to_framebuffer_when_snapshot_stale(
    tmp_path: Path,
) -> None:
    """A stale (or absent) sidecar PNG drops to a direct framebuffer read,
    which the standard-library encoder turns into a PNG without PIL."""
    snap = tmp_path / "lcd-snapshot.png"  # never created → absent
    display_routes._snap_cache.clear()
    fb_png = b"\x89PNG\r\n\x1a\nfb-frame"
    with patch.object(display_routes, "LCD_SNAPSHOT_PATH", snap), \
         patch.object(
             display_routes, "_resolve_fb_path", return_value="/dev/fb0",
         ), \
         patch.object(
             display_routes, "render_framebuffer_png", return_value=fb_png,
         ) as mock_fb:
        out = display_routes._render_snapshot_png(240, 160)
    assert out == fb_png
    mock_fb.assert_called_once_with("/dev/fb0")


# ── page ------------------------------------------------------------


def test_page_get_reports_no_page_when_no_display_published_state(
    client: TestClient, tmp_path: Path,
) -> None:
    """No display service, no active page: never a default the panel is not showing."""
    with patch.object(display_routes, "LCD_STATE_PATH", tmp_path / "absent.json"):
        resp = client.get("/api/v1/display/page")
    assert resp.status_code == 200
    assert resp.json() == {"available": False, "active_page": None, "modal_stack": []}


def test_page_get_reads_the_navigator_state(client: TestClient, tmp_path: Path) -> None:
    state = _state_with_routes(tmp_path, ["dashboard", "video"])
    with patch.object(display_routes, "LCD_STATE_PATH", state):
        resp = client.get("/api/v1/display/page")
    assert resp.json() == {"available": True, "active_page": "dashboard", "modal_stack": []}


def _state_with_routes(tmp_path: Path, routes: list[str]) -> Path:
    """An lcd-state.json as the navigator writes it at start."""
    state = tmp_path / "lcd-state.json"
    state.write_text(json.dumps({
        "active_page_id": "dashboard",
        "modal_stack": [],
        "route_ids": routes,
    }))
    return state


def test_page_post_writes_request_file(
    client: TestClient, tmp_path: Path,
) -> None:
    """POST /page writes a request blob the OLED service can pick up."""
    target = tmp_path / "lcd-page-request.json"
    state = _state_with_routes(tmp_path, ["dashboard", "video"])
    with patch.object(display_routes, "LCD_PAGE_REQUEST_PATH", target), \
         patch.object(display_routes, "LCD_STATE_PATH", state):
        resp = client.post(
            "/api/v1/display/page",
            json={"page": "video"},
        )
    assert resp.status_code == 200
    assert resp.json() == {"ok": True, "active_page": "video"}
    assert target.exists()
    blob = json.loads(target.read_text())
    assert blob["page"] == "video"
    assert "requested_at_ms" in blob


def test_page_post_accepts_every_route_the_navigator_registered(
    client: TestClient, tmp_path: Path,
) -> None:
    """The reserved plugin page and the channel-hops tab are requestable
    because the navigator lists them, not because a copy here does."""
    target = tmp_path / "lcd-page-request.json"
    state = _state_with_routes(
        tmp_path, ["dashboard", "channel_hops", "plugin"],
    )
    with patch.object(display_routes, "LCD_PAGE_REQUEST_PATH", target), \
         patch.object(display_routes, "LCD_STATE_PATH", state):
        for page in ("plugin", "channel_hops"):
            resp = client.post("/api/v1/display/page", json={"page": page})
            assert resp.status_code == 200, page
            assert json.loads(target.read_text())["page"] == page


def test_page_post_rejects_unknown_id(
    client: TestClient, tmp_path: Path,
) -> None:
    state = _state_with_routes(tmp_path, ["dashboard", "video"])
    with patch.object(display_routes, "LCD_STATE_PATH", state):
        resp = client.post(
            "/api/v1/display/page",
            json={"page": "deepspace"},
        )
    assert resp.status_code == 400
    body = resp.json()
    detail = body["detail"]
    assert detail["error"] == "unknown_page"
    assert detail["page"] == "deepspace"
    assert detail["valid"] == ["dashboard", "video"]


def test_page_post_refuses_when_no_navigator_published_routes(
    client: TestClient, tmp_path: Path,
) -> None:
    target = tmp_path / "lcd-page-request.json"
    with patch.object(display_routes, "LCD_PAGE_REQUEST_PATH", target), \
         patch.object(display_routes, "LCD_STATE_PATH", tmp_path / "absent.json"):
        resp = client.post("/api/v1/display/page", json={"page": "video"})
    assert resp.status_code == 503
    assert resp.json()["detail"]["error"] == "display_not_running"
    assert not target.exists()
