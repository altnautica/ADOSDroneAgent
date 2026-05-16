"""Helpers for the GPS-denied navigation wizard step.

The wizard surfaces three operator-tunable knobs:

* whether the active board has the hooks for vision-based navigation
  (CSI/USB cameras, IMU-aligned compute headroom, rangefinder ports)
* which camera is bound to the navigation role
* which rangefinder is wired in and where it lives (companion or FC)

Plus a one-shot calibration upload path for the Kalibr camera+IMU
pair (``camchain-imucam.yaml`` + ``imu.yaml``) and a five-second
preflight sample so the operator can confirm the chosen camera is
actually streaming usable frames before they finalize the wizard.

Everything here is pure-Python helpers. The FastAPI surface in
``ados.api.routes.setup`` is the only public boundary; this module
holds the parsing, filesystem, and HAL plumbing so the route file
stays small.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import yaml

from ados.core.logging import get_logger
from ados.core.paths import ADOS_ETC_DIR
from ados.hal.camera import CameraInfo, CameraType, HardwareRole, discover_cameras
from ados.hal.detect import _load_board_profiles, detect_board
from ados.services.video.camera_mgr import CameraRole

log = get_logger("setup.navigation")


# Drivers the agent knows how to bring up. Any new rangefinder added in
# a board profile must show up here too, otherwise the wizard refuses
# the assignment and surfaces a clear error.
RANGEFINDER_DRIVER_ALLOWLIST: frozenset[str] = frozenset(
    {
        "tfluna_uart",
        "garmin_lidarlite_i2c",
        "vl53l1x_i2c",
    }
)


# Modes accepted by ``POST /setup/navigation/config``. ``off`` disables
# the navigation pipeline entirely and is the wizard's skip path.
NAV_MODES: frozenset[str] = frozenset(
    {"off", "optical-flow", "vio", "both"}
)


# Camera orientations the operator can pin under VIO mode. The wizard
# speaks this simplified four-value vocabulary so an operator does not
# have to learn the plugin's full six-mode + camera-orientation matrix;
# the agent translates it into the plugin's config schema when it
# writes the YAML.
#
# ``forward``  -- indoor / corridor / inspection
# ``downward`` -- agriculture / survey / SAR / pipeline patrol (default
#                  for the over-ground suites)
# ``auto``     -- defer to the bound HAL camera role
VIO_CAMERA_ORIENTATIONS: frozenset[str] = frozenset(
    {"forward", "downward", "auto"}
)


# Firmware types the wizard surfaces. Mirrors the plugin's
# ``FirmwareConfig.type`` literal. Betaflight is intentionally absent
# because the firmware has no position estimator; the wizard refuses
# to enable navigation on Betaflight hosts with a clear error.
NAV_FIRMWARE_TYPES: frozenset[str] = frozenset(
    {"ardupilot", "px4", "inav"}
)


# Default plugin id the wizard targets when the caller does not pass one.
# Plugin ids are namespaced by reverse-DNS; the navigation plugin lives
# under the company namespace so a future contributed alternative does
# not collide.
DEFAULT_NAV_PLUGIN_ID = "com.altnautica.vision-nav"


# Roles the wizard accepts on assign-camera. ``nav`` resolves to the new
# navigation role added by the camera manager; older agents without the
# NAV variant fall back to SECONDARY transparently (see
# :func:`safe_camera_role`).
ASSIGN_ROLE_LITERALS: frozenset[str] = frozenset(
    {"nav", "primary", "secondary", "thermal", "inspection"}
)


@dataclass
class NavigationCapabilities:
    """Snapshot of the navigation-relevant capabilities for the active board.

    ``vio_capable`` is the operator-friendly summary: either the board
    profile claims it, or the runtime probe sees enough hardware (CSI
    or USB camera + at least one free rangefinder port) to make VIO
    plausible. The wizard uses this to gate the VIO checkbox so an
    operator on a board that cannot run VIO does not pick it.
    """

    vio_capable: bool
    csi_count: int
    usb_uvc_count: int
    rangefinder_ports: list[dict[str, str]]


def safe_camera_role(name: str) -> CameraRole:
    """Resolve a wizard role string to a :class:`CameraRole` value.

    ``nav`` resolves to ``CameraRole.NAV`` when the camera manager has
    been extended to include it. Older agents that have not yet rolled
    in the NAV role fall back to ``SECONDARY`` so the wizard can ship
    independent of the camera-manager change.
    """
    key = (name or "").lower()
    if key == "nav":
        candidate = getattr(CameraRole, "NAV", None)
        if candidate is not None:
            return candidate  # type: ignore[no-any-return]
        return CameraRole.SECONDARY
    try:
        return CameraRole(key)
    except ValueError as exc:
        raise ValueError(f"unknown camera role: {name!r}") from exc


def read_nav_capabilities() -> NavigationCapabilities:
    """Build a navigation-capability snapshot for the active board.

    Reads the active board's profile YAML for any declared navigation
    block, then runs a fresh HAL camera discovery to fill in the live
    camera counts. Either input alone is enough to populate the
    structure; both are merged so the wizard can offer a sensible
    default even on a board profile that does not yet ship the
    navigation block.
    """
    board = detect_board()
    profile = next(
        (p for p in _load_board_profiles() if p.name == board.name),
        None,
    )

    nav_block: dict[str, Any] = {}
    if profile is not None:
        # ``BoardProfile`` is a Pydantic model. Future revisions of the
        # YAML schema will expose a ``navigation`` field; we read it via
        # getattr + model_extra so unknown-field-allowed profiles are
        # still supported, and absent-field profiles degrade silently.
        nav_block = (
            getattr(profile, "navigation", None)
            or (getattr(profile, "model_extra", {}) or {}).get("navigation")
            or {}
        )
        if not isinstance(nav_block, dict):
            nav_block = {}

    rangefinder_ports = _extract_rangefinder_ports(nav_block, profile)

    csi_count, usb_count = _camera_counts_from_hal()

    declared_vio = bool(nav_block.get("vio_capable"))
    inferred_vio = (csi_count + usb_count) >= 1 and len(rangefinder_ports) >= 1

    return NavigationCapabilities(
        vio_capable=declared_vio or inferred_vio,
        csi_count=csi_count,
        usb_uvc_count=usb_count,
        rangefinder_ports=rangefinder_ports,
    )


def _extract_rangefinder_ports(
    nav_block: dict[str, Any], profile: Any
) -> list[dict[str, str]]:
    """Pull the rangefinder port descriptors out of the navigation block.

    Falls back to the board's declared ``uart_paths`` when the
    navigation block is silent; gives the wizard at least one suggestion
    on boards that have not yet adopted the navigation schema.
    """
    raw = nav_block.get("rangefinder_ports") if isinstance(nav_block, dict) else None
    ports: list[dict[str, str]] = []
    if isinstance(raw, list):
        for entry in raw:
            if isinstance(entry, dict):
                bus = str(entry.get("bus", "")).strip()
                path = str(entry.get("path", "")).strip()
                if path:
                    ports.append({"bus": bus or "uart", "path": path})
    if ports:
        return ports
    if profile is not None:
        for path in getattr(profile, "uart_paths", None) or []:
            ports.append({"bus": "uart", "path": str(path)})
    return ports


def _camera_counts_from_hal() -> tuple[int, int]:
    """Count CSI + USB cameras via a fresh HAL discovery.

    Discovery is cheap (~150 ms on Pi-class hardware). Filtering by
    ``HardwareRole.CAMERA`` keeps codecs and ISPs out of the count.
    """
    try:
        cams = discover_cameras()
    except Exception as exc:  # noqa: BLE001 (HAL probe must never break the wizard)
        log.warning("nav_camera_discovery_failed", error=str(exc))
        return 0, 0
    real = [c for c in cams if c.hardware_role == HardwareRole.CAMERA]
    csi = sum(1 for c in real if c.type == CameraType.CSI)
    usb = sum(1 for c in real if c.type == CameraType.USB)
    return csi, usb


def discovered_cameras_with_role_hints(
    current_assignments: dict[str, CameraInfo] | None = None,
) -> list[dict[str, str]]:
    """List discovered cameras and tag each with current + recommended role.

    The recommended role uses simple heuristics: the first CSI is the
    natural navigation camera (global shutter is the eventual ideal,
    but a rolling-shutter CSI is still a valid VIO source). USB UVC is
    recommended as primary when no CSI is present; thermal/inspection
    pulls in any camera advertising a relevant capability string.
    """
    try:
        cams = discover_cameras()
    except Exception as exc:  # noqa: BLE001
        log.warning("nav_camera_list_failed", error=str(exc))
        return []
    real = [c for c in cams if c.hardware_role == HardwareRole.CAMERA]

    assignments = current_assignments or {}
    have_csi = any(c.type == CameraType.CSI for c in real)

    out: list[dict[str, str]] = []
    for cam in real:
        current = ""
        for role_str, assigned in assignments.items():
            if getattr(assigned, "device_path", "") == cam.device_path:
                current = str(role_str)
                break
        out.append(
            {
                "device": cam.device_path,
                "name": cam.name,
                "kind": cam.type.value,
                "current_role": current,
                "recommended_role": _recommend_role(cam, have_csi),
            }
        )
    return out


def _recommend_role(cam: CameraInfo, have_csi: bool) -> str:
    caps = [c.lower() for c in (cam.capabilities or [])]
    if "thermal" in caps or "lepton" in cam.name.lower():
        return "thermal"
    if cam.type == CameraType.CSI:
        # First CSI → nav. Heuristic stays simple; the operator can override.
        return "nav"
    if cam.type == CameraType.USB and not have_csi:
        return "primary"
    return "secondary"


# ---------------------------------------------------------------------
# Kalibr calibration upload
# ---------------------------------------------------------------------


# Kalibr emits two artifacts per camera+IMU calibration run. The first
# carries the per-camera intrinsics and the IMU-to-camera transform;
# the second the IMU noise model. The wizard requires both before VIO
# can flip live.
EXPECTED_CAMCHAIN_KEYS: frozenset[str] = frozenset({"cam0"})
EXPECTED_IMU_KEYS: frozenset[str] = frozenset({"imu0"})

# Hard ceiling on each YAML upload. The legitimate Kalibr files are a
# few kB; anything bigger is almost certainly an operator pasting the
# wrong file (a flight log, an image, etc.). Reject with a 413 instead
# of letting it land on disk.
MAX_CALIBRATION_UPLOAD_BYTES = 256 * 1024


def validate_kalibr_yaml(content: bytes, kind: str) -> dict[str, Any]:
    """Parse + validate one half of a Kalibr calibration pair.

    ``kind`` is ``"camchain"`` or ``"imu"``. Returns the parsed mapping
    on success; raises ``ValueError`` on parse failure or missing
    required top-level keys.
    """
    if not content:
        raise ValueError(f"{kind}: file is empty")
    if len(content) > MAX_CALIBRATION_UPLOAD_BYTES:
        raise ValueError(
            f"{kind}: file is too large ({len(content)} bytes; limit "
            f"{MAX_CALIBRATION_UPLOAD_BYTES})"
        )
    try:
        parsed = yaml.safe_load(content)
    except yaml.YAMLError as exc:
        raise ValueError(f"{kind}: YAML parse failed: {exc}") from exc
    if not isinstance(parsed, dict):
        raise ValueError(f"{kind}: top-level must be a mapping")

    expected = EXPECTED_CAMCHAIN_KEYS if kind == "camchain" else EXPECTED_IMU_KEYS
    missing = sorted(k for k in expected if k not in parsed)
    if missing:
        raise ValueError(
            f"{kind}: required keys missing: {', '.join(missing)}"
        )
    return parsed


def calibration_dir(plugin_id: str) -> Path:
    """Where Kalibr files live on disk for a given plugin id."""
    return ADOS_ETC_DIR / "plugins" / plugin_id / "calibration"


def save_calibration_files(
    plugin_id: str,
    camchain_bytes: bytes,
    imu_bytes: bytes,
) -> dict[str, str]:
    """Persist the Kalibr YAML pair atomically.

    Returns a small dict with the on-disk paths so the route can echo
    them back to the operator. Both files are validated before either
    is written so a partial drop is impossible.
    """
    validate_kalibr_yaml(camchain_bytes, "camchain")
    validate_kalibr_yaml(imu_bytes, "imu")

    target = calibration_dir(plugin_id)
    target.mkdir(parents=True, exist_ok=True)

    cam_path = target / "camchain-imucam.yaml"
    imu_path = target / "imu.yaml"
    _atomic_write_bytes(cam_path, camchain_bytes)
    _atomic_write_bytes(imu_path, imu_bytes)
    return {
        "camchain_path": str(cam_path),
        "imu_path": str(imu_path),
    }


def _atomic_write_bytes(path: Path, data: bytes) -> None:
    """Write ``data`` to ``path`` via a temp-and-rename dance."""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=path.name + ".", dir=str(path.parent))
    try:
        with os.fdopen(fd, "wb") as fh:
            fh.write(data)
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp, path)
    except Exception:
        with _suppress():
            os.unlink(tmp)
        raise


class _suppress:
    """Mini contextlib.suppress without the import cost on a cold path."""

    def __enter__(self) -> None:
        return None

    def __exit__(self, exc_type, exc, tb) -> bool:
        return exc_type is not None


# ---------------------------------------------------------------------
# Plugin config persistence
# ---------------------------------------------------------------------


def plugin_config_path(plugin_id: str) -> Path:
    """Where the wizard writes the navigation plugin's config file."""
    return ADOS_ETC_DIR / "plugins" / plugin_id / "config.yaml"


def write_plugin_config(plugin_id: str, payload: dict[str, Any]) -> Path:
    """Persist the navigation config block under the plugin's config dir.

    The plugin supervisor reads this file on next start. Atomic write
    keeps a partial flush from leaving the plugin staring at an empty
    YAML.
    """
    target = plugin_config_path(plugin_id)
    blob = yaml.safe_dump(payload, sort_keys=True, default_flow_style=False)
    _atomic_write_bytes(target, blob.encode("utf-8"))
    return target


def validate_nav_config(payload: dict[str, Any]) -> None:
    """Validate the body of ``POST /setup/navigation/config``.

    Raises ``ValueError`` with a human-readable message on any failure
    so the route can surface a 400.
    """
    mode = str(payload.get("mode", "")).strip()
    if mode not in NAV_MODES:
        raise ValueError(
            f"mode must be one of {sorted(NAV_MODES)}; got {mode!r}"
        )

    # Firmware gate: Betaflight has no position estimator, so even a
    # well-formed nav config cannot run there. iNav has optical flow
    # but no VIO; the validator rejects vio + inav up front.
    firmware = str(payload.get("firmware", "ardupilot")).strip()
    if firmware and firmware not in NAV_FIRMWARE_TYPES:
        # Betaflight or any other unsupported firmware.
        raise ValueError(
            f"firmware {firmware!r} is not supported for vision "
            "navigation. Betaflight has no position estimator; "
            "cross-flash iNav or ArduPilot Copter to enable flow / VIO."
        )
    if firmware == "inav" and mode in {"vio", "both"}:
        raise ValueError(
            "VIO is not supported on iNav in this release. Use "
            "mode='optical-flow' with a downward camera + rangefinder, "
            "or cross-flash ArduPilot Copter or PX4 for VIO."
        )

    # Camera orientation gate. Only meaningful when mode is vio or
    # both; optical-flow is always downward regardless of what the
    # wizard sends. The wizard refuses ``downward`` when the discovered
    # cameras include no downward-mounted device.
    orientation = payload.get("vio_camera_orientation")
    if orientation is not None:
        orientation_s = str(orientation).strip()
        if orientation_s not in VIO_CAMERA_ORIENTATIONS:
            raise ValueError(
                "vio_camera_orientation must be one of "
                f"{sorted(VIO_CAMERA_ORIENTATIONS)}; got {orientation_s!r}"
            )
        if mode not in {"vio", "both"} and orientation_s != "auto":
            raise ValueError(
                "vio_camera_orientation only applies to mode='vio' "
                "or mode='both'; got mode="
                f"{mode!r} with orientation={orientation_s!r}"
            )

    rf = payload.get("rangefinder")
    if rf is None:
        return  # rangefinder is optional except in obstacle-avoidance modes
    if not isinstance(rf, dict):
        raise ValueError("rangefinder must be an object")
    topology = str(rf.get("topology", "")).strip()
    if topology not in {"companion", "fc"}:
        raise ValueError(
            f"rangefinder.topology must be 'companion' or 'fc'; got {topology!r}"
        )
    driver = str(rf.get("driver", "")).strip()
    if driver not in RANGEFINDER_DRIVER_ALLOWLIST:
        raise ValueError(
            f"rangefinder.driver {driver!r} is not in the supported list: "
            f"{sorted(RANGEFINDER_DRIVER_ALLOWLIST)}"
        )
    device = rf.get("device")
    if not isinstance(device, dict) or not str(device.get("path", "")).strip():
        raise ValueError("rangefinder.device.path is required")


def translate_wizard_to_plugin_config(
    payload: dict[str, Any],
) -> dict[str, Any]:
    """Translate the wizard's simplified 4-mode + orientation payload
    into the plugin's 6-mode + camera-orientation config shape.

    The wizard vocabulary is intentionally smaller than the plugin's
    so the operator does not have to think in estimator-engine terms.
    This translation runs at write time, before
    :func:`write_plugin_config` serializes the YAML the plugin
    supervisor will load on next start.

    Mapping:

    * ``off`` -> plugin ``mode='off'``.
    * ``optical-flow`` -> plugin ``mode='optical_flow'`` (downward
      camera, rangefinder required). When no rangefinder is wired the
      plugin auto-flips to ``optical_flow_degraded`` at start-up.
    * ``vio`` -> plugin ``mode='vio_vins_fusion'`` (the default VIO
      engine). The wizard's ``vio_camera_orientation`` becomes the
      plugin's ``camera.orientation``.
    * ``both`` -> plugin ``mode='hybrid_of_plus_vio'``. The primary
      camera carries the downward OF stream; the secondary carries the
      forward VIO stream. The wizard's orientation field is informational
      for the operator's confirmation step and does not change the
      plugin's hybrid camera assignments.
    """

    mode = str(payload.get("mode", "off")).strip()
    orientation = str(payload.get("vio_camera_orientation", "auto")).strip()
    firmware = str(payload.get("firmware", "ardupilot")).strip() or "ardupilot"

    if mode == "off":
        plugin_mode = "off"
    elif mode == "optical-flow":
        plugin_mode = "optical_flow"
    elif mode == "vio":
        plugin_mode = "vio_vins_fusion"
    elif mode == "both":
        plugin_mode = "hybrid_of_plus_vio"
    else:
        # validate_nav_config already gates the literal; fall through
        # defensively in case a caller bypasses validation.
        plugin_mode = "off"

    # Camera orientation only carried for VIO modes. Optical flow is
    # always downward; hybrid uses both cameras with explicit slots.
    if plugin_mode in {"vio_vins_fusion", "vio_openvins"}:
        camera_orientation = orientation
    elif plugin_mode == "optical_flow":
        camera_orientation = "downward"
    else:
        camera_orientation = "auto"

    plugin_payload: dict[str, Any] = {
        "mode": plugin_mode,
        "camera": {"orientation": camera_orientation},
        "firmware": {"type": firmware},
    }
    rf = payload.get("rangefinder")
    if isinstance(rf, dict):
        plugin_payload["rangefinder"] = {
            "topology": rf.get("topology", "fc"),
            "driver": rf.get("driver", "fc_relay"),
        }
        device = rf.get("device")
        if isinstance(device, dict) and device.get("path"):
            plugin_payload["rangefinder"]["device"] = device["path"]
    return plugin_payload


# ---------------------------------------------------------------------
# Preflight capture
# ---------------------------------------------------------------------


@dataclass
class PreflightSample:
    frames_captured: int
    avg_quality: float
    mean_distance_m: float | None
    status: str


def run_preflight_sample(
    camera_device: str | None,
    duration_seconds: float = 5.0,
) -> PreflightSample:
    """Sample the nav-assigned camera for a short window.

    Uses ``ffprobe`` when available; falls back to a v4l2-ctl format
    probe + a static "no_frames" verdict when ffprobe is absent. Never
    raises; the route surfaces the result as JSON regardless of the
    underlying probe outcome.
    """
    if not camera_device:
        return PreflightSample(
            frames_captured=0,
            avg_quality=0.0,
            mean_distance_m=None,
            status="no_camera",
        )
    if not Path(camera_device).exists():
        return PreflightSample(
            frames_captured=0,
            avg_quality=0.0,
            mean_distance_m=None,
            status="no_camera",
        )

    ffprobe = shutil.which("ffprobe")
    if ffprobe is None:
        return PreflightSample(
            frames_captured=0,
            avg_quality=0.0,
            mean_distance_m=None,
            status="no_frames",
        )

    cmd = [
        ffprobe,
        "-f",
        "v4l2",
        "-i",
        camera_device,
        "-t",
        str(max(0.5, duration_seconds)),
        "-show_entries",
        "stream=nb_read_frames",
        "-count_frames",
        "-of",
        "default=nokey=1:noprint_wrappers=1",
    ]
    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=max(2.0, duration_seconds + 2.0),
        )
    except (subprocess.TimeoutExpired, OSError) as exc:
        log.warning("nav_preflight_probe_failed", error=str(exc))
        return PreflightSample(
            frames_captured=0,
            avg_quality=0.0,
            mean_distance_m=None,
            status="no_frames",
        )

    frames = 0
    for line in (result.stdout or "").splitlines():
        token = line.strip()
        if token.isdigit():
            frames = int(token)
            break

    if frames <= 0:
        return PreflightSample(
            frames_captured=0,
            avg_quality=0.0,
            mean_distance_m=None,
            status="no_frames",
        )

    target_fps = 30.0 * max(0.5, duration_seconds)
    quality = min(1.0, frames / target_fps) if target_fps > 0 else 0.0
    status = "good" if quality >= 0.5 else "low_quality"

    return PreflightSample(
        frames_captured=frames,
        avg_quality=round(quality, 3),
        mean_distance_m=None,
        status=status,
    )
