"""CameraRole.NAV + exclusive-claim semantics for camera_mgr.

Covers the schema-v2 additions: NAV role enum, exclusive claim on
behalf of a plugin id, conflict raised on cross-plugin reclaim,
idempotent same-plugin reclaim, release semantics, and ``to_dict``
surfacing of the ``claimed_by`` field for the GCS badge.
"""

from __future__ import annotations

import pytest

from ados.hal.camera import CameraInfo, CameraType
from ados.services.video.camera_mgr import (
    CameraManager,
    CameraRole,
    RoleConflict,
)


def _cam(name: str = "uvc-down", path: str = "/dev/video2") -> CameraInfo:
    return CameraInfo(
        name=name,
        type=CameraType.USB,
        device_path=path,
        width=640,
        height=480,
        capabilities=["mjpeg"],
    )


def test_nav_role_enum_value() -> None:
    """NAV must serialize as ``"nav"`` so the GCS i18n and the
    capability surface stay in sync."""
    assert CameraRole.NAV.value == "nav"


def test_exclusive_claim_happy_path() -> None:
    mgr = CameraManager()
    cam = _cam()
    mgr.set_cameras([cam])

    mgr.assign_role_exclusive(cam, CameraRole.NAV, plugin_id="com.example.vision-nav")

    assert mgr.get_by_role(CameraRole.NAV) is cam
    assert mgr.claimed_by(cam.device_path) == "com.example.vision-nav"


def test_exclusive_claim_idempotent_same_plugin() -> None:
    """Re-claiming with the same plugin id must succeed (idempotent)."""
    mgr = CameraManager()
    cam = _cam()
    mgr.set_cameras([cam])
    mgr.assign_role_exclusive(cam, CameraRole.NAV, plugin_id="com.example.vision-nav")
    # Second call should not raise; useful on plugin restart.
    mgr.assign_role_exclusive(cam, CameraRole.NAV, plugin_id="com.example.vision-nav")
    assert mgr.claimed_by(cam.device_path) == "com.example.vision-nav"


def test_exclusive_claim_conflict_across_plugins() -> None:
    mgr = CameraManager()
    cam = _cam()
    mgr.set_cameras([cam])
    mgr.assign_role_exclusive(cam, CameraRole.NAV, plugin_id="com.example.vision-nav")
    with pytest.raises(RoleConflict) as info:
        mgr.assign_role_exclusive(
            cam, CameraRole.NAV, plugin_id="com.other.tracker"
        )
    exc = info.value
    assert exc.device_path == cam.device_path
    assert exc.role == CameraRole.NAV
    assert exc.current_holder == "com.example.vision-nav"
    assert exc.requested_by == "com.other.tracker"


def test_setup_wizard_assign_blocked_by_claim() -> None:
    """Non-exclusive ``assign_role`` must refuse to overwrite a
    plugin's exclusive claim. The setup wizard hits this path when an
    operator tries to repurpose a vision-claimed camera."""
    mgr = CameraManager()
    cam = _cam()
    mgr.set_cameras([cam])
    mgr.assign_role_exclusive(cam, CameraRole.NAV, plugin_id="com.example.vision-nav")
    with pytest.raises(RoleConflict):
        mgr.assign_role(cam, CameraRole.PRIMARY)


def test_release_claim_returns_true_for_holder() -> None:
    mgr = CameraManager()
    cam = _cam()
    mgr.set_cameras([cam])
    mgr.assign_role_exclusive(cam, CameraRole.NAV, plugin_id="com.example.vision-nav")
    released = mgr.release_claim(cam.device_path, "com.example.vision-nav")
    assert released is True
    assert mgr.claimed_by(cam.device_path) is None


def test_release_claim_returns_false_for_wrong_plugin() -> None:
    mgr = CameraManager()
    cam = _cam()
    mgr.set_cameras([cam])
    mgr.assign_role_exclusive(cam, CameraRole.NAV, plugin_id="com.example.vision-nav")
    released = mgr.release_claim(cam.device_path, "com.other.tracker")
    assert released is False
    assert mgr.claimed_by(cam.device_path) == "com.example.vision-nav"


def test_release_claim_idempotent_when_unclaimed() -> None:
    mgr = CameraManager()
    cam = _cam()
    mgr.set_cameras([cam])
    assert mgr.release_claim(cam.device_path, "any.plugin") is False


def test_to_dict_surfaces_claimed_by() -> None:
    mgr = CameraManager()
    primary = _cam(name="csi-front", path="/dev/video0")
    nav = _cam(name="uvc-down", path="/dev/video2")
    mgr.set_cameras([primary, nav])
    mgr.assign_role(primary, CameraRole.PRIMARY)
    mgr.assign_role_exclusive(nav, CameraRole.NAV, plugin_id="com.example.vision-nav")

    snap = mgr.to_dict()
    by_path = {c["device_path"]: c for c in snap["cameras"]}
    assert by_path["/dev/video0"]["claimed_by"] is None
    assert by_path["/dev/video2"]["claimed_by"] == "com.example.vision-nav"

    assignments = snap["assignments"]
    assert assignments["primary"]["claimed_by"] is None
    assert assignments["nav"]["claimed_by"] == "com.example.vision-nav"


def test_assign_role_after_release_succeeds() -> None:
    """The wizard regains control once the plugin releases its claim."""
    mgr = CameraManager()
    cam = _cam()
    mgr.set_cameras([cam])
    mgr.assign_role_exclusive(cam, CameraRole.NAV, plugin_id="com.example.vision-nav")
    mgr.release_claim(cam.device_path, "com.example.vision-nav")
    # Wizard repurposes the camera as SECONDARY.
    mgr.assign_role(cam, CameraRole.SECONDARY)
    assert mgr.get_by_role(CameraRole.SECONDARY) is cam


def test_assign_role_exclusive_rejects_empty_plugin_id() -> None:
    mgr = CameraManager()
    cam = _cam()
    mgr.set_cameras([cam])
    with pytest.raises(ValueError):
        mgr.assign_role_exclusive(cam, CameraRole.NAV, plugin_id="")
