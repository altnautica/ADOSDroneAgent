"""Tests for the /api/v1/setup/navigation/* wizard endpoints.

Five routes shipped:

* ``GET  /navigation/capabilities`` board nav summary
* ``GET  /navigation/cameras``      discovery + role hint
* ``POST /navigation/assign-camera`` bind device to role
* ``POST /navigation/calibration``  multipart Kalibr upload
* ``POST /navigation/config``       persist mode + rangefinder
* ``GET  /navigation/preflight``    five-second capture sample

The tests stub the camera manager + plugin destination directory so the
suite runs on macOS dev hosts without root or /etc/ados write access.
"""

from __future__ import annotations

import io
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from ados.hal.camera import CameraInfo, CameraType, HardwareRole
from ados.services.video.camera_mgr import CameraManager, CameraRole
from ados.setup import navigation_helpers as nav_helpers
from tests.api_runtime_utils import build_api_runtime

# Inline Kalibr YAML samples — enough fields to satisfy the validator.
_CAMCHAIN_OK = b"""cam0:
  camera_model: pinhole
  intrinsics: [320.0, 320.0, 320.0, 240.0]
  resolution: [640, 480]
"""

_IMU_OK = b"""imu0:
  accelerometer_noise_density: 0.01
  accelerometer_random_walk: 0.0002
  gyroscope_noise_density: 0.005
  gyroscope_random_walk: 4.0e-06
  update_rate: 200.0
"""


def _make_camera(device: str = "/dev/video0", kind: CameraType = CameraType.CSI) -> CameraInfo:
    return CameraInfo(
        name=f"test-{device}",
        type=kind,
        device_path=device,
        width=1280,
        height=720,
        capabilities=["h264"],
        hardware_role=HardwareRole.CAMERA,
    )


def _wire_camera_mgr(*cameras: CameraInfo) -> MagicMock:
    """Return a pipeline stub whose `_camera_mgr` is a real CameraManager."""
    mgr = CameraManager()
    mgr.set_cameras(list(cameras))
    pipeline = MagicMock()
    pipeline._camera_mgr = mgr
    pipeline.camera_mgr = mgr  # tolerated alias
    return pipeline


@pytest.fixture
def cameras() -> list[CameraInfo]:
    return [_make_camera("/dev/video0", CameraType.CSI), _make_camera("/dev/video1", CameraType.USB)]


@pytest.fixture
def agent_app(cameras: list[CameraInfo]):
    return build_api_runtime(video_pipeline=_wire_camera_mgr(*cameras))


@pytest.fixture
def client(agent_app):
    return TestClient(create_app(agent_app))


@pytest.fixture
def plugin_etc(monkeypatch, tmp_path: Path) -> Path:
    """Redirect /etc/ados/plugins/<id>/ writes into tmp_path."""
    fake_etc = tmp_path / "etc" / "ados"
    fake_etc.mkdir(parents=True, exist_ok=True)
    monkeypatch.setattr(nav_helpers, "ADOS_ETC_DIR", fake_etc)
    return fake_etc


# ---------------------------------------------------------------------------
# GET /navigation/capabilities
# ---------------------------------------------------------------------------


def test_capabilities_returns_expected_shape(client) -> None:
    with patch.object(nav_helpers, "discover_cameras", return_value=[_make_camera()]):
        resp = client.get("/api/v1/setup/navigation/capabilities")
    assert resp.status_code == 200
    body = resp.json()
    assert set(body.keys()) >= {
        "vio_capable",
        "csi_count",
        "usb_uvc_count",
        "rangefinder_ports",
    }
    assert isinstance(body["rangefinder_ports"], list)


# ---------------------------------------------------------------------------
# GET /navigation/cameras
# ---------------------------------------------------------------------------


def test_cameras_lists_discovered(client, cameras) -> None:
    with patch.object(nav_helpers, "discover_cameras", return_value=cameras):
        resp = client.get("/api/v1/setup/navigation/cameras")
    assert resp.status_code == 200
    body = resp.json()
    devices = {c["device"] for c in body["cameras"]}
    assert devices == {"/dev/video0", "/dev/video1"}
    # First CSI is recommended as the nav camera.
    csi = next(c for c in body["cameras"] if c["device"] == "/dev/video0")
    assert csi["recommended_role"] == "nav"


# ---------------------------------------------------------------------------
# POST /navigation/assign-camera
# ---------------------------------------------------------------------------


def test_assign_camera_binds_role(client, agent_app) -> None:
    resp = client.post(
        "/api/v1/setup/navigation/assign-camera",
        json={"device_path": "/dev/video0", "role": "nav"},
    )
    assert resp.status_code == 200
    body = resp.json()
    assert body["ok"] is True
    mgr = agent_app.video_pipeline_handle._camera_mgr
    # The "nav" role resolves to NAV if available, otherwise SECONDARY.
    role = nav_helpers.safe_camera_role("nav")
    assert mgr.get_by_role(role).device_path == "/dev/video0"


def test_assign_camera_unknown_device_returns_404(client) -> None:
    resp = client.post(
        "/api/v1/setup/navigation/assign-camera",
        json={"device_path": "/dev/never", "role": "nav"},
    )
    assert resp.status_code == 404


def test_assign_camera_no_pipeline_returns_503() -> None:
    runtime = build_api_runtime(video_pipeline=None)
    c = TestClient(create_app(runtime))
    resp = c.post(
        "/api/v1/setup/navigation/assign-camera",
        json={"device_path": "/dev/video0", "role": "nav"},
    )
    assert resp.status_code == 503


def test_assign_camera_exclusive_role_conflict_returns_409(client, agent_app, cameras) -> None:
    mgr = agent_app.video_pipeline_handle._camera_mgr
    # Pre-bind thermal to camera 1 so a request to bind camera 0 to
    # thermal must surface the exclusive-role guard.
    mgr.assign_role(cameras[1], CameraRole.THERMAL)
    resp = client.post(
        "/api/v1/setup/navigation/assign-camera",
        json={"device_path": "/dev/video0", "role": "thermal"},
    )
    assert resp.status_code == 409


def test_assign_nav_installs_exclusive_claim(client, agent_app) -> None:
    """A clean NAV bind installs an exclusive claim for the nav plugin
    so the plugin can reattach after a restart without losing the
    camera reservation."""
    resp = client.post(
        "/api/v1/setup/navigation/assign-camera",
        json={"device_path": "/dev/video0", "role": "nav"},
    )
    assert resp.status_code == 200
    body = resp.json()
    assert body["ok"] is True
    assert body["data"]["forced"] is False
    mgr = agent_app.video_pipeline_handle._camera_mgr
    from ados.setup import navigation_helpers as nh

    assert mgr.claimed_by("/dev/video0") == nh.DEFAULT_NAV_PLUGIN_ID


def test_assign_nav_plugin_claim_returns_structured_409(client, agent_app) -> None:
    """When a third-party plugin already claims the camera, the route
    returns a 409 with a structured body so the GCS can prompt the
    operator with the current holder."""
    mgr = agent_app.video_pipeline_handle._camera_mgr
    # Some other plugin already owns /dev/video0.
    cam = next(c for c in mgr.cameras if c.device_path == "/dev/video0")
    mgr.assign_role_exclusive(cam, CameraRole.NAV, plugin_id="com.other.tracker")

    resp = client.post(
        "/api/v1/setup/navigation/assign-camera",
        json={"device_path": "/dev/video0", "role": "nav"},
    )
    assert resp.status_code == 409
    body = resp.json()
    # FastAPI wraps non-string detail under the "detail" key.
    detail = body["detail"]
    assert detail["error"] == "role_conflict"
    assert detail["device_path"] == "/dev/video0"
    assert detail["current_plugin"] == "com.other.tracker"
    assert detail["requested_role"] == "nav"
    assert "message" in detail


def test_assign_nav_force_true_overrides_plugin_claim(client, agent_app) -> None:
    """``force=true`` drops the existing claim and installs the wizard's
    claim on behalf of the navigation plugin."""
    mgr = agent_app.video_pipeline_handle._camera_mgr
    cam = next(c for c in mgr.cameras if c.device_path == "/dev/video0")
    mgr.assign_role_exclusive(cam, CameraRole.NAV, plugin_id="com.other.tracker")

    resp = client.post(
        "/api/v1/setup/navigation/assign-camera?force=true",
        json={"device_path": "/dev/video0", "role": "nav"},
    )
    assert resp.status_code == 200
    body = resp.json()
    assert body["data"]["forced"] is True

    from ados.setup import navigation_helpers as nh

    assert mgr.claimed_by("/dev/video0") == nh.DEFAULT_NAV_PLUGIN_ID


def test_assign_secondary_role_with_force_drops_claim_without_replacing(
    client, agent_app
) -> None:
    """Forcing a non-NAV role drops the existing plugin claim but does
    not install a new wizard claim (only NAV is held exclusively by
    the wizard)."""
    mgr = agent_app.video_pipeline_handle._camera_mgr
    cam = next(c for c in mgr.cameras if c.device_path == "/dev/video0")
    mgr.assign_role_exclusive(cam, CameraRole.NAV, plugin_id="com.other.tracker")

    resp = client.post(
        "/api/v1/setup/navigation/assign-camera?force=true",
        json={"device_path": "/dev/video0", "role": "secondary"},
    )
    assert resp.status_code == 200
    assert mgr.claimed_by("/dev/video0") is None
    assert mgr.get_by_role(CameraRole.SECONDARY).device_path == "/dev/video0"


# ---------------------------------------------------------------------------
# POST /navigation/calibration
# ---------------------------------------------------------------------------


def test_calibration_persists_pair(client, plugin_etc: Path) -> None:
    files = {
        "camchain": ("camchain-imucam.yaml", io.BytesIO(_CAMCHAIN_OK), "text/yaml"),
        "imu": ("imu.yaml", io.BytesIO(_IMU_OK), "text/yaml"),
    }
    resp = client.post(
        "/api/v1/setup/navigation/calibration",
        files=files,
    )
    assert resp.status_code == 200
    body = resp.json()
    assert body["ok"] is True
    cam_path = Path(body["data"]["camchain_path"])
    imu_path = Path(body["data"]["imu_path"])
    assert cam_path.is_file()
    assert imu_path.is_file()
    assert cam_path.read_bytes() == _CAMCHAIN_OK
    assert imu_path.read_bytes() == _IMU_OK


def test_calibration_missing_required_key_rejects(client, plugin_etc: Path) -> None:
    bad_camchain = b"cam99:\n  intrinsics: [1, 2]\n"
    files = {
        "camchain": ("camchain-imucam.yaml", io.BytesIO(bad_camchain), "text/yaml"),
        "imu": ("imu.yaml", io.BytesIO(_IMU_OK), "text/yaml"),
    }
    resp = client.post(
        "/api/v1/setup/navigation/calibration",
        files=files,
    )
    assert resp.status_code == 400
    # Partial-drop guard: neither file should land on disk.
    cal_dir = nav_helpers.calibration_dir(nav_helpers.DEFAULT_NAV_PLUGIN_ID)
    assert not (cal_dir / "imu.yaml").exists()


def test_calibration_uses_custom_plugin_id(client, plugin_etc: Path) -> None:
    files = {
        "camchain": ("camchain-imucam.yaml", io.BytesIO(_CAMCHAIN_OK), "text/yaml"),
        "imu": ("imu.yaml", io.BytesIO(_IMU_OK), "text/yaml"),
    }
    resp = client.post(
        "/api/v1/setup/navigation/calibration",
        data={"plugin_id": "com.example.alt-nav"},
        files=files,
    )
    assert resp.status_code == 200
    target = nav_helpers.calibration_dir("com.example.alt-nav")
    assert (target / "camchain-imucam.yaml").is_file()


# ---------------------------------------------------------------------------
# POST /navigation/config
# ---------------------------------------------------------------------------


def test_config_writes_plugin_yaml(client, plugin_etc: Path) -> None:
    payload = {
        "mode": "optical-flow",
        "rangefinder": {
            "topology": "companion",
            "driver": "tfluna_uart",
            "device": {"path": "/dev/ttyS3", "baud": 115200},
        },
    }
    resp = client.post("/api/v1/setup/navigation/config", json=payload)
    assert resp.status_code == 200
    body = resp.json()
    assert body["ok"] is True
    cfg_path = Path(body["data"]["config_path"])
    assert cfg_path.is_file()
    text = cfg_path.read_text()
    # The wizard speaks "optical-flow" but the plugin's persisted YAML
    # uses the plugin's native "optical_flow" mode key after the
    # wizard-to-plugin translation step.
    assert "mode: optical_flow" in text
    assert "tfluna_uart" in text
    # Optical-flow always forces downward orientation in the translation.
    assert "orientation: downward" in text


def test_config_vio_with_downward_orientation_writes_plugin_vio(
    client, plugin_etc: Path
) -> None:
    """VIO + downward camera (the over-ground default) translates into
    the plugin's vio_vins_fusion mode with camera.orientation=downward.
    This is the agriculture / survey / SAR / pipeline-patrol path."""
    payload = {
        "mode": "vio",
        "vio_camera_orientation": "downward",
        "firmware": "ardupilot",
    }
    resp = client.post("/api/v1/setup/navigation/config", json=payload)
    assert resp.status_code == 200
    cfg_path = Path(resp.json()["data"]["config_path"])
    text = cfg_path.read_text()
    assert "mode: vio_vins_fusion" in text
    assert "orientation: downward" in text
    assert "type: ardupilot" in text


def test_config_vio_with_forward_orientation_writes_plugin_vio(
    client, plugin_etc: Path
) -> None:
    """VIO + forward camera (the indoor / corridor default) translates
    into the plugin's vio_vins_fusion mode with camera.orientation=forward."""
    payload = {
        "mode": "vio",
        "vio_camera_orientation": "forward",
        "firmware": "px4",
    }
    resp = client.post("/api/v1/setup/navigation/config", json=payload)
    assert resp.status_code == 200
    cfg_path = Path(resp.json()["data"]["config_path"])
    text = cfg_path.read_text()
    assert "mode: vio_vins_fusion" in text
    assert "orientation: forward" in text
    assert "type: px4" in text


def test_config_both_mode_writes_hybrid(client, plugin_etc: Path) -> None:
    """The wizard's 'both' translates to the plugin's hybrid_of_plus_vio."""
    payload = {
        "mode": "both",
        "vio_camera_orientation": "forward",
        "firmware": "ardupilot",
    }
    resp = client.post("/api/v1/setup/navigation/config", json=payload)
    assert resp.status_code == 200
    cfg_path = Path(resp.json()["data"]["config_path"])
    text = cfg_path.read_text()
    assert "mode: hybrid_of_plus_vio" in text


def test_config_inav_with_optical_flow_accepted(
    client, plugin_etc: Path
) -> None:
    """iNav + optical flow is supported. Plugin emits OPTICAL_FLOW_RAD
    which iNav 7.0+ consumes when opflow_hardware=MAVLINK."""
    payload = {
        "mode": "optical-flow",
        "firmware": "inav",
        "rangefinder": {
            "topology": "companion",
            "driver": "tfluna_uart",
            "device": {"path": "/dev/ttyS3"},
        },
    }
    resp = client.post("/api/v1/setup/navigation/config", json=payload)
    assert resp.status_code == 200
    cfg_path = Path(resp.json()["data"]["config_path"])
    text = cfg_path.read_text()
    assert "type: inav" in text
    assert "mode: optical_flow" in text


def test_config_inav_with_vio_rejected(client, plugin_etc: Path) -> None:
    """iNav + VIO is rejected. iNav's external position injection EKF
    integration is not VIO-grade in 7.x."""
    payload = {"mode": "vio", "firmware": "inav"}
    resp = client.post("/api/v1/setup/navigation/config", json=payload)
    assert resp.status_code == 400
    detail = resp.json()["detail"]
    assert "iNav" in detail or "inav" in detail.lower()


def test_config_inav_with_both_rejected(client, plugin_etc: Path) -> None:
    """Hybrid mode requires VIO; iNav cannot run VIO so 'both' is
    also rejected."""
    payload = {"mode": "both", "firmware": "inav"}
    resp = client.post("/api/v1/setup/navigation/config", json=payload)
    assert resp.status_code == 400


def test_config_betaflight_rejected_by_pydantic(
    client, plugin_etc: Path
) -> None:
    """Betaflight is intentionally absent from the firmware literal so
    pydantic rejects the payload before it ever reaches the validator.
    The 422 (vs the 400 the validator would return) is the expected
    FastAPI behavior for schema violations."""
    payload = {"mode": "optical-flow", "firmware": "betaflight"}
    resp = client.post("/api/v1/setup/navigation/config", json=payload)
    assert resp.status_code == 422


def test_config_orientation_on_optical_flow_rejected(
    client, plugin_etc: Path
) -> None:
    """vio_camera_orientation only applies to vio/both. Setting forward
    or downward on optical-flow is an operator error and surfaces a 400."""
    payload = {
        "mode": "optical-flow",
        "vio_camera_orientation": "forward",
    }
    resp = client.post("/api/v1/setup/navigation/config", json=payload)
    assert resp.status_code == 400


def test_config_orientation_auto_passes_on_optical_flow(
    client, plugin_etc: Path
) -> None:
    """vio_camera_orientation='auto' is a no-op on optical-flow and
    must not be rejected. It only carries meaning under VIO modes."""
    payload = {
        "mode": "optical-flow",
        "vio_camera_orientation": "auto",
        "rangefinder": {
            "topology": "companion",
            "driver": "tfluna_uart",
            "device": {"path": "/dev/ttyS3"},
        },
    }
    resp = client.post("/api/v1/setup/navigation/config", json=payload)
    assert resp.status_code == 200


def test_config_unknown_rangefinder_driver_rejects(client, plugin_etc: Path) -> None:
    payload = {
        "mode": "vio",
        "vio_camera_orientation": "downward",
        "firmware": "ardupilot",
        "rangefinder": {
            "topology": "companion",
            "driver": "definitely_not_a_driver",
            "device": {"path": "/dev/ttyS3"},
        },
    }
    resp = client.post("/api/v1/setup/navigation/config", json=payload)
    assert resp.status_code == 400


def test_config_mode_off_marks_step_skipped(client, plugin_etc: Path, monkeypatch) -> None:
    captured: list[str] = []

    def fake_mark_skipped(step_id: str):
        captured.append(step_id)

    monkeypatch.setattr(
        "ados.api.routes.setup.setup_state.mark_skipped",
        fake_mark_skipped,
    )
    resp = client.post("/api/v1/setup/navigation/config", json={"mode": "off"})
    assert resp.status_code == 200
    assert captured == ["navigation"]


# ---------------------------------------------------------------------------
# GET /navigation/preflight
# ---------------------------------------------------------------------------


def test_preflight_returns_no_camera_when_pipeline_empty() -> None:
    runtime = build_api_runtime(video_pipeline=None)
    c = TestClient(create_app(runtime))
    resp = c.get("/api/v1/setup/navigation/preflight")
    assert resp.status_code == 200
    assert resp.json()["status"] == "no_camera"


def test_preflight_returns_sample_when_camera_assigned(client, agent_app, cameras) -> None:
    mgr = agent_app.video_pipeline_handle._camera_mgr
    nav_role = nav_helpers.safe_camera_role("nav")
    mgr.assign_role(cameras[0], nav_role)

    fake = nav_helpers.PreflightSample(
        frames_captured=150,
        avg_quality=1.0,
        mean_distance_m=None,
        status="good",
    )
    with patch.object(nav_helpers, "run_preflight_sample", return_value=fake):
        resp = client.get("/api/v1/setup/navigation/preflight")
    assert resp.status_code == 200
    body = resp.json()
    assert body["status"] == "good"
    assert body["frames_captured"] == 150
