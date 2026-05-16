"""GPS-denied navigation wizard step routes."""

from __future__ import annotations

import asyncio

from fastapi import APIRouter, File, Form, HTTPException, Query, UploadFile

from ados.api.deps import get_agent_app
from ados.services.video.camera_mgr import RoleConflict
from ados.setup import navigation_helpers as nav_helpers
from ados.setup import state as setup_state
from ados.setup.models import SetupActionResult

from ._common import log
from ._models import (
    NavigationAssignCameraRequest,
    NavigationCameraEntry,
    NavigationCamerasResponse,
    NavigationCapabilitiesResponse,
    NavigationConfigRequest,
    NavigationPreflightResponse,
)

router = APIRouter()


def _resolve_camera_mgr():
    """Return the live camera manager from the video pipeline, or None.

    The wizard runs in the API process; the camera manager lives on the
    video pipeline instance. Returning None when the pipeline has not
    started yet lets the routes degrade to a fresh HAL discovery for
    the read paths and surface a clear 503 on the write paths.
    """
    try:
        pipeline = get_agent_app().video_pipeline()
    except Exception:
        return None
    if pipeline is None:
        return None
    return getattr(pipeline, "_camera_mgr", None) or getattr(
        pipeline, "camera_mgr", None
    )


@router.get(
    "/navigation/capabilities",
    response_model=NavigationCapabilitiesResponse,
)
async def get_navigation_capabilities() -> NavigationCapabilitiesResponse:
    """Return the active board's navigation capability summary."""
    snap = nav_helpers.read_nav_capabilities()
    return NavigationCapabilitiesResponse(
        vio_capable=snap.vio_capable,
        csi_count=snap.csi_count,
        usb_uvc_count=snap.usb_uvc_count,
        rangefinder_ports=snap.rangefinder_ports,
    )


@router.get(
    "/navigation/cameras",
    response_model=NavigationCamerasResponse,
)
async def list_navigation_cameras() -> NavigationCamerasResponse:
    """Discovered cameras + current + recommended role for each."""
    mgr = _resolve_camera_mgr()
    assignments: dict[str, object] = {}
    if mgr is not None:
        try:
            assignments = {
                role.value: cam
                for role, cam in mgr.assignments.items()
            }
        except Exception as exc:  # noqa: BLE001
            log.warning("nav_assignments_read_failed", error=str(exc))
    entries = nav_helpers.discovered_cameras_with_role_hints(assignments or None)
    return NavigationCamerasResponse(
        cameras=[NavigationCameraEntry(**e) for e in entries]
    )


@router.post("/navigation/assign-camera", response_model=SetupActionResult)
async def assign_navigation_camera(
    request: NavigationAssignCameraRequest,
    force: bool = Query(
        default=False,
        description=(
            "When true, drop any existing plugin claim on the target "
            "camera and reassign. Operator-confirmed override path "
            "the GCS uses after a 409 prompt."
        ),
    ),
) -> SetupActionResult:
    """Bind a discovered camera to a role.

    Two 409 paths exist:

    * Plugin claim conflict: the target camera is exclusively claimed
      by a plugin (typically vision-nav) and the operator is trying
      to repurpose it via the wizard. The response body carries
      ``error: "role_conflict"`` with the current holder so the GCS
      can render a confirm dialog. Pass ``?force=true`` to override.
    * Role-already-bound conflict (``thermal``, ``inspection``): the
      requested role is already pointed at a different device. The
      operator must unassign the role first; ``force=true`` does NOT
      override this path because it is not a plugin claim, it is the
      operator's own previous wizard choice.
    """
    mgr = _resolve_camera_mgr()
    if mgr is None:
        raise HTTPException(
            status_code=503,
            detail=(
                "Video pipeline is not running. Start video before assigning "
                "a navigation camera."
            ),
        )

    target = None
    for cam in getattr(mgr, "cameras", []) or []:
        if getattr(cam, "device_path", "") == request.device_path:
            target = cam
            break
    if target is None:
        raise HTTPException(
            status_code=404,
            detail=f"No discovered camera at device path {request.device_path!r}.",
        )

    try:
        role = nav_helpers.safe_camera_role(request.role)
    except ValueError as exc:
        raise HTTPException(status_code=400, detail=str(exc)) from exc

    # Exclusive-role guard: thermal + inspection cannot be silently
    # reassigned to a different device. Operator must unassign first.
    existing = None
    try:
        existing = mgr.get_by_role(role)
    except Exception:  # noqa: BLE001
        existing = None
    if (
        request.role in {"thermal", "inspection"}
        and existing is not None
        and getattr(existing, "device_path", "") != request.device_path
    ):
        raise HTTPException(
            status_code=409,
            detail=(
                f"Role {request.role!r} is already bound to "
                f"{getattr(existing, 'device_path', 'another camera')!r}. "
                "Unassign it first."
            ),
        )

    # NAV is the only role where the wizard installs an exclusive claim
    # on behalf of the navigation plugin so the plugin can survive a
    # restart without losing the camera reservation. Other roles stay
    # non-exclusive (the wizard can repoint them freely).
    new_claim_plugin_id: str | None = (
        nav_helpers.DEFAULT_NAV_PLUGIN_ID if request.role == "nav" else None
    )

    if force:
        # Operator-confirmed override path: drop whatever claim is on
        # the device and reassign atomically under the manager's lock.
        dropped = mgr.reassign_role_exclusive(target, role, new_claim_plugin_id)
        log.warning(
            "nav_assign_camera_forced",
            device_path=request.device_path,
            requested_role=request.role,
            dropped_holder=dropped,
            new_holder=new_claim_plugin_id,
        )
    else:
        try:
            if new_claim_plugin_id is not None:
                mgr.assign_role_exclusive(target, role, new_claim_plugin_id)
            else:
                mgr.assign_role(target, role)
        except RoleConflict as conflict:
            log.warning(
                "nav_assign_camera_role_conflict",
                device_path=request.device_path,
                requested_role=request.role,
                current_holder=conflict.current_holder,
            )
            raise HTTPException(
                status_code=409,
                detail={
                    "error": "role_conflict",
                    "device_path": request.device_path,
                    "current_role": role.value,
                    "current_plugin": conflict.current_holder,
                    "requested_role": request.role,
                    "message": (
                        f"Camera {request.device_path} is currently claimed "
                        f"by {conflict.current_holder!r}. Retry with "
                        "force=true to reassign."
                    ),
                },
            ) from conflict
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc
        except Exception as exc:  # noqa: BLE001
            raise HTTPException(status_code=500, detail=str(exc)) from exc

    return SetupActionResult(
        ok=True,
        message=f"Camera {request.device_path} assigned to role {request.role}.",
        data={
            "device_path": request.device_path,
            "role": request.role,
            "forced": force,
        },
    )


@router.post("/navigation/calibration", response_model=SetupActionResult)
async def upload_navigation_calibration(
    camchain: UploadFile = File(...),
    imu: UploadFile = File(...),
    plugin_id: str | None = Form(default=None),
) -> SetupActionResult:
    """Persist a Kalibr camchain + IMU YAML pair on disk.

    Both files are validated (size, YAML parseable, required top-level
    keys present) before either lands; a partial drop is impossible.
    The destination is
    ``/etc/ados/plugins/<plugin_id>/calibration/`` with ``plugin_id``
    defaulting to the navigation plugin's canonical id when the form
    field is omitted.
    """
    pid = (plugin_id or nav_helpers.DEFAULT_NAV_PLUGIN_ID).strip()
    if not pid or "/" in pid or pid.startswith("."):
        raise HTTPException(status_code=400, detail="plugin_id is invalid.")

    try:
        camchain_bytes = await camchain.read()
        imu_bytes = await imu.read()
    except Exception as exc:  # noqa: BLE001
        raise HTTPException(
            status_code=400,
            detail=f"Could not read uploaded files: {exc}",
        ) from exc

    try:
        result = nav_helpers.save_calibration_files(
            pid, camchain_bytes, imu_bytes
        )
    except ValueError as exc:
        raise HTTPException(status_code=400, detail=str(exc)) from exc
    except OSError as exc:
        raise HTTPException(
            status_code=500,
            detail=f"Could not persist calibration files: {exc}",
        ) from exc

    return SetupActionResult(
        ok=True,
        message="Calibration files saved.",
        data={"plugin_id": pid, **result},
    )


@router.post("/navigation/config", response_model=SetupActionResult)
async def configure_navigation(
    request: NavigationConfigRequest,
) -> SetupActionResult:
    """Persist the navigation plugin's config and the wizard step state.

    Writes the YAML config to
    ``/etc/ados/plugins/<plugin_id>/config.yaml``. ``mode="off"``
    additionally marks the navigation wizard step as ``skipped`` so the
    operator can advance the wizard without engaging the plugin.
    """
    pid = (request.plugin_id or nav_helpers.DEFAULT_NAV_PLUGIN_ID).strip()
    if not pid or "/" in pid or pid.startswith("."):
        raise HTTPException(status_code=400, detail="plugin_id is invalid.")

    payload = request.model_dump(exclude_none=True)
    payload.pop("plugin_id", None)
    try:
        nav_helpers.validate_nav_config(payload)
    except ValueError as exc:
        raise HTTPException(status_code=400, detail=str(exc)) from exc

    try:
        cfg_path = nav_helpers.write_plugin_config(pid, payload)
    except OSError as exc:
        raise HTTPException(
            status_code=500,
            detail=f"Could not write plugin config: {exc}",
        ) from exc

    if request.mode == "off":
        setup_state.mark_skipped("navigation")

    return SetupActionResult(
        ok=True,
        message=f"Navigation configured ({request.mode}).",
        data={"plugin_id": pid, "config_path": str(cfg_path)},
    )


@router.get(
    "/navigation/preflight",
    response_model=NavigationPreflightResponse,
)
async def run_navigation_preflight() -> NavigationPreflightResponse:
    """Capture a short sample on the camera bound to the nav role.

    Falls back to the primary camera when no NAV-bound camera is found,
    so a wizard that just configured the assignment can still preview a
    frame count before the operator finalizes.
    """
    mgr = _resolve_camera_mgr()
    device: str | None = None
    if mgr is not None:
        try:
            nav_role = nav_helpers.safe_camera_role("nav")
            bound = mgr.get_by_role(nav_role)
            if bound is not None:
                device = getattr(bound, "device_path", None)
            if device is None:
                primary = mgr.get_primary()
                if primary is not None:
                    device = getattr(primary, "device_path", None)
        except Exception as exc:  # noqa: BLE001
            log.warning("nav_preflight_camera_lookup_failed", error=str(exc))

    sample = await asyncio.to_thread(nav_helpers.run_preflight_sample, device)
    return NavigationPreflightResponse(
        frames_captured=sample.frames_captured,
        avg_quality=sample.avg_quality,
        mean_distance_m=sample.mean_distance_m,
        status=sample.status,  # type: ignore[arg-type]
    )
