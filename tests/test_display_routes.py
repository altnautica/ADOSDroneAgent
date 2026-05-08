"""Tests for the ``/api/v1/display/*`` REST surface.

Covers:

* ``POST /calibrate/start`` arms the shared session, returns the
  five targets, and bumps the generation counter.
* ``POST /calibrate/sample`` accepts samples in order, rejects
  out-of-order submits.
* ``POST /calibrate/save`` runs the affine fit, persists the file on
  acceptance, surfaces the residual on rejection.
* ``POST /calibrate/skip`` writes the skip marker.
* ``GET /calibrate/status`` reflects on-disk + in-progress state.
* ``GET /page`` reads the navigator's persisted JSON.
* ``POST /page`` validates the page id and writes the request file.
* ``GET /touches`` returns the recent-touches ring buffer.
"""

from __future__ import annotations

import json
from pathlib import Path
from unittest.mock import patch

import pytest
from fastapi.testclient import TestClient

from ados.api.routes import display as display_routes
from ados.api.server import create_app
from ados.services.ui.touch import recent as recent_touches_module
from ados.services.ui.touch.session import (
    STEP_COUNT,
    TARGETS,
    get_session_registry,
)
from ados.services.ui.touch.transform import load as load_calib
from tests.api_runtime_utils import build_api_runtime


@pytest.fixture
def session_save_path(tmp_path: Path) -> Path:
    return tmp_path / "touch.calib"


@pytest.fixture(autouse=True)
def _reset_session_and_ring(session_save_path: Path) -> None:
    """Clean session + ring buffer between tests so they never bleed state."""
    registry = get_session_registry()
    registry.reset()
    recent_touches_module.clear()
    yield
    registry.reset()
    recent_touches_module.clear()


@pytest.fixture
def client(tmp_path: Path) -> TestClient:
    runtime = build_api_runtime()
    fastapi_app = create_app(runtime)
    return TestClient(fastapi_app)


# ── calibrate -------------------------------------------------------


def test_calibrate_start_returns_targets_and_bumps_generation(
    client: TestClient,
) -> None:
    snap0 = get_session_registry().snapshot()
    resp = client.post("/api/v1/display/calibrate/start")
    assert resp.status_code == 200
    body = resp.json()
    assert body["target_count"] == STEP_COUNT
    assert body["current_step"] == 0
    targets = body["targets"]
    assert len(targets) == STEP_COUNT
    for i, (tx, ty) in enumerate(TARGETS):
        assert targets[i] == {"idx": i, "x": tx, "y": ty}
    snap1 = get_session_registry().snapshot()
    assert snap1.in_progress is True
    assert snap1.generation == snap0.generation + 1
    assert body["job_id"].startswith("cal-")


def test_calibrate_sample_accepts_in_order_and_rejects_out_of_order(
    client: TestClient,
) -> None:
    client.post("/api/v1/display/calibrate/start")
    # In-order sample at step 0.
    r1 = client.post(
        "/api/v1/display/calibrate/sample",
        json={"step": 0, "x_raw": 100, "y_raw": 100},
    )
    assert r1.status_code == 200
    assert r1.json() == {"accepted": True, "next_step": 1, "complete": False}
    # Out-of-order: step 0 again — should be rejected.
    r2 = client.post(
        "/api/v1/display/calibrate/sample",
        json={"step": 0, "x_raw": 200, "y_raw": 200},
    )
    assert r2.status_code == 200
    body2 = r2.json()
    assert body2["accepted"] is False
    assert body2["next_step"] == 1


def test_calibrate_save_persists_affine_round_trip(
    client: TestClient, session_save_path: Path,
) -> None:
    """A full start -> 5 samples -> save round-trip writes a real file."""
    # Patch the registry's save path so we don't touch /etc/ados.
    registry = get_session_registry()
    registry.start(save_path=session_save_path)
    samples = [
        (100, 100),
        (3995, 100),
        (2047, 2047),
        (100, 3995),
        (3995, 3995),
    ]
    for i, (xr, yr) in enumerate(samples):
        r = client.post(
            "/api/v1/display/calibrate/sample",
            json={"step": i, "x_raw": xr, "y_raw": yr},
        )
        assert r.status_code == 200, r.text
        assert r.json()["accepted"] is True
    # Save should produce a real touch.calib that load() can parse.
    resp = client.post("/api/v1/display/calibrate/save")
    assert resp.status_code == 200
    body = resp.json()
    assert body["ok"] is True
    assert body["rms_residual_px"] is not None
    assert body["rms_residual_px"] < 5.0
    on_disk = load_calib(session_save_path)
    assert on_disk is not None


def test_calibrate_save_rejects_high_rms(
    client: TestClient, session_save_path: Path,
) -> None:
    """All samples at the panel center fits a degenerate matrix."""
    registry = get_session_registry()
    registry.start(save_path=session_save_path)
    for i in range(STEP_COUNT):
        r = client.post(
            "/api/v1/display/calibrate/sample",
            json={"step": i, "x_raw": 2047, "y_raw": 2047},
        )
        assert r.status_code == 200
    resp = client.post("/api/v1/display/calibrate/save")
    assert resp.status_code == 200
    body = resp.json()
    assert body["ok"] is False
    assert body["error"] is not None
    # The file must NOT be on disk after a rejection.
    assert load_calib(session_save_path) is None


def test_calibrate_skip_writes_marker_and_ends_session(
    client: TestClient, session_save_path: Path,
) -> None:
    registry = get_session_registry()
    registry.start(save_path=session_save_path)
    resp = client.post("/api/v1/display/calibrate/skip")
    assert resp.status_code == 200
    assert resp.json() == {"ok": True}
    snap = registry.snapshot()
    assert snap.in_progress is False
    # The skip marker file should exist with calibrated=False.
    assert session_save_path.exists()
    blob = json.loads(session_save_path.read_text())
    assert blob["calibrated"] is False
    assert blob.get("skipped") is True


def test_calibrate_status_reflects_in_progress_and_disk(
    client: TestClient, session_save_path: Path,
) -> None:
    # Pre-state: nothing on disk, no session.
    with patch.object(display_routes, "TOUCH_CALIB_PATH", session_save_path):
        r0 = client.get("/api/v1/display/calibrate/status")
        assert r0.status_code == 200
        body0 = r0.json()
        assert body0["calibrated"] is False
        assert body0["in_progress"] is False

        # Arm a session — current_step appears.
        client.post("/api/v1/display/calibrate/start")
        r1 = client.get("/api/v1/display/calibrate/status")
        body1 = r1.json()
        assert body1["in_progress"] is True
        assert body1["current_step"] == 0


# ── snapshot --------------------------------------------------------


def test_snapshot_returns_404_when_no_lcd(client: TestClient) -> None:
    """Without a display.conf, the endpoint returns 404 with the canonical detail."""
    with patch.object(display_routes, "_lcd_is_bound", return_value=False):
        resp = client.get("/api/v1/display/snapshot")
    assert resp.status_code == 404
    assert resp.json()["detail"] == "no_lcd_bound"


def test_snapshot_returns_png_when_lcd_present(client: TestClient) -> None:
    """When _render_snapshot_png returns bytes, the response is image/png."""
    fake_png = b"\x89PNG\r\n\x1a\nfake-bytes"
    with patch.object(display_routes, "_lcd_is_bound", return_value=True), \
         patch.object(display_routes, "_render_snapshot_png", return_value=fake_png):
        resp = client.get("/api/v1/display/snapshot?width=240&height=160")
    assert resp.status_code == 200
    assert resp.headers["content-type"] == "image/png"
    assert resp.content == fake_png


def test_snapshot_caches_repeated_requests(client: TestClient) -> None:
    """Two snapshots within the cache TTL produce a single render call."""
    # Reset the cache so this test starts clean.
    display_routes._snap_cache.clear()
    fake_png = b"\x89PNG\r\n\x1a\nfake-bytes"
    with patch.object(display_routes, "_lcd_is_bound", return_value=True), \
         patch.object(
             display_routes, "_render_snapshot_png", return_value=fake_png,
         ) as mock_render:
        client.get("/api/v1/display/snapshot?width=240&height=160")
        client.get("/api/v1/display/snapshot?width=240&height=160")
    # Cache holds the first result; the second request reuses it.
    assert mock_render.call_count == 1


# ── page ------------------------------------------------------------


def test_page_get_returns_active_page_default(client: TestClient) -> None:
    """When no lcd-state.json exists, the route defaults to dashboard."""
    with patch.object(
        display_routes,
        "_read_lcd_state",
        return_value={"active_page": "dashboard", "modal_stack": []},
    ):
        resp = client.get("/api/v1/display/page")
    assert resp.status_code == 200
    body = resp.json()
    assert body == {"active_page": "dashboard", "modal_stack": []}


def test_page_post_writes_request_file(
    client: TestClient, tmp_path: Path,
) -> None:
    """POST /page writes a request blob the OLED service can pick up."""
    target = tmp_path / "lcd-page-request.json"
    with patch.object(display_routes, "LCD_PAGE_REQUEST_PATH", target):
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


def test_page_post_rejects_unknown_id(client: TestClient) -> None:
    resp = client.post(
        "/api/v1/display/page",
        json={"page": "deepspace"},
    )
    assert resp.status_code == 400
    body = resp.json()
    detail = body["detail"]
    assert detail["error"] == "unknown_page"
    assert detail["page"] == "deepspace"
    assert "valid" in detail


# ── touches ---------------------------------------------------------


def test_touches_returns_empty_list_initially(client: TestClient) -> None:
    resp = client.get("/api/v1/display/touches")
    assert resp.status_code == 200
    assert resp.json() == {"events": []}


def test_touches_returns_recorded_events_oldest_first(
    client: TestClient,
) -> None:
    recent_touches_module.record_touch(
        kind="tap", x=10, y=20, page="dashboard", timestamp_ms=1_000,
    )
    recent_touches_module.record_touch(
        kind="swipe", x=50, y=60, page="video", timestamp_ms=2_000,
    )
    resp = client.get("/api/v1/display/touches")
    assert resp.status_code == 200
    events = resp.json()["events"]
    assert len(events) == 2
    assert events[0]["kind"] == "tap"
    assert events[0]["t"] == 1_000
    assert events[1]["kind"] == "swipe"
    assert events[1]["t"] == 2_000


def test_touches_filters_by_since_ms(client: TestClient) -> None:
    recent_touches_module.record_touch(
        kind="tap", x=1, y=2, page="dashboard", timestamp_ms=1_000,
    )
    recent_touches_module.record_touch(
        kind="long_press", x=3, y=4, page="more", timestamp_ms=2_000,
    )
    resp = client.get("/api/v1/display/touches?since_ms=1500")
    assert resp.status_code == 200
    events = resp.json()["events"]
    assert len(events) == 1
    assert events[0]["kind"] == "long_press"
