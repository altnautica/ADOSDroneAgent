"""Tests for the local status surface's camera presence + USB-recovery fields.

The cloud heartbeat carries ``cameraState``; the LAN-direct status did not, so a
locally-paired operator got no camera-missing signal. ``_read_camera_status``
closes that gap by reading the same two sidecars the video pipeline + supervisor
write, staleness-gated.
"""

from __future__ import annotations

import json
import time

import ados.core.paths as paths
from ados.api.routes.status import _read_camera_status


def _write(path, obj):
    path.write_text(json.dumps(obj))


def test_absent_sidecars_yield_no_camera_keys(tmp_path, monkeypatch):
    monkeypatch.setattr(paths, "CAMERA_STATE_JSON", tmp_path / "camera-state.json")
    monkeypatch.setattr(
        paths, "CAMERA_USB_RECOVERY_JSON", tmp_path / "camera-usb-recovery.json"
    )
    out = _read_camera_status()
    assert out == {}


def test_fresh_missing_camera_surfaces_state(tmp_path, monkeypatch):
    state = tmp_path / "camera-state.json"
    _write(state, {"state": "missing", "updated_at_unix": time.time()})
    monkeypatch.setattr(paths, "CAMERA_STATE_JSON", state)
    monkeypatch.setattr(
        paths, "CAMERA_USB_RECOVERY_JSON", tmp_path / "camera-usb-recovery.json"
    )
    out = _read_camera_status()
    assert out["cameraState"] == "missing"
    assert "cameraUsbRecovery" not in out


def test_stale_state_is_dropped(tmp_path, monkeypatch):
    state = tmp_path / "camera-state.json"
    _write(state, {"state": "ready", "updated_at_unix": time.time() - 10_000})
    monkeypatch.setattr(paths, "CAMERA_STATE_JSON", state)
    monkeypatch.setattr(
        paths, "CAMERA_USB_RECOVERY_JSON", tmp_path / "camera-usb-recovery.json"
    )
    assert _read_camera_status() == {}


def test_recovery_block_surfaces_and_clamps(tmp_path, monkeypatch):
    monkeypatch.setattr(paths, "CAMERA_STATE_JSON", tmp_path / "camera-state.json")
    rec = tmp_path / "camera-usb-recovery.json"
    _write(
        rec,
        {
            "camera_usb_recovery_state": "needs_hub_reset",
            "case": "absent",
            "attempts": 1,
            "max_attempts": 3,
            "camera_present": False,
            "expected": True,
            "ppps_capable": False,
            "updated_at_unix": time.time(),
        },
    )
    monkeypatch.setattr(paths, "CAMERA_USB_RECOVERY_JSON", rec)
    out = _read_camera_status()
    block = out["cameraUsbRecovery"]
    assert block["state"] == "needs_hub_reset"
    assert block["expected"] is True
    assert block["maxAttempts"] == 3
    assert block["pppsCapable"] is False


def test_invalid_recovery_state_is_dropped(tmp_path, monkeypatch):
    monkeypatch.setattr(paths, "CAMERA_STATE_JSON", tmp_path / "camera-state.json")
    rec = tmp_path / "camera-usb-recovery.json"
    _write(
        rec,
        {"camera_usb_recovery_state": "bogus", "updated_at_unix": time.time()},
    )
    monkeypatch.setattr(paths, "CAMERA_USB_RECOVERY_JSON", rec)
    assert _read_camera_status() == {}
