"""Camera manager — maps discovered cameras to roles for the video pipeline."""

from __future__ import annotations

import threading
from enum import StrEnum

from ados.core.logging import get_logger
from ados.hal.camera import CameraInfo, CameraType, HardwareRole

log = get_logger("video.camera_mgr")


class CameraRole(StrEnum):
    PRIMARY = "primary"
    SECONDARY = "secondary"
    THERMAL = "thermal"
    INSPECTION = "inspection"
    NAV = "nav"
    """Vision-navigation role. Cameras in this role feed an
    optical-flow or VIO pipeline (typically a downward-facing global
    shutter sensor) and are exclusively claimed by a single plugin so
    the setup wizard cannot reassign them mid-flight."""


class RoleConflict(Exception):
    """Raised when an exclusive role claim collides with an existing
    claim held by a different plugin. Carries the device path, the
    role, the current holder, and the requesting plugin so the GCS
    can render an actionable error."""

    def __init__(
        self,
        device_path: str,
        role: "CameraRole",
        current_holder: str,
        requested_by: str,
    ) -> None:
        super().__init__(
            f"camera {device_path} role {role.value} exclusively "
            f"claimed by {current_holder!r}; "
            f"{requested_by!r} cannot reassign"
        )
        self.device_path = device_path
        self.role = role
        self.current_holder = current_holder
        self.requested_by = requested_by


class CameraManager:
    """Manages camera-to-role assignment.

    Cameras are discovered by the HAL layer and then assigned roles here.  The
    pipeline uses the role assignment to decide which camera feeds which stream.

    Two assignment paths exist. ``assign_role`` is the non-exclusive
    path used by the setup wizard and the auto-assign heuristic; it
    overwrites whatever was previously assigned to the role. Plugins
    that need a stable camera reservation across wizard runs call
    ``assign_role_exclusive`` instead, which records the claiming
    plugin id and refuses to reassign while the claim is held. The
    setup wizard checks ``claimed_by`` on each camera before
    offering it to the operator.
    """

    def __init__(self) -> None:
        self._assignments: dict[CameraRole, CameraInfo] = {}
        self._cameras: list[CameraInfo] = []
        self._claims: dict[str, str] = {}
        """device_path -> plugin_id for exclusive claims."""
        self._state_lock = threading.Lock()

    @property
    def cameras(self) -> list[CameraInfo]:
        return list(self._cameras)

    @property
    def assignments(self) -> dict[CameraRole, CameraInfo]:
        return dict(self._assignments)

    def set_cameras(self, cameras: list[CameraInfo]) -> None:
        """Replace the known camera list (from discovery)."""
        self._cameras = list(cameras)
        log.info("cameras_updated", count=len(cameras))

    def assign_role(self, camera: CameraInfo, role: CameraRole) -> None:
        """Assign a role to a camera.

        If the role was previously assigned to another camera, it is replaced.
        Non-exclusive: this path is used by the setup wizard and the auto-assign
        heuristic. Plugins that need an exclusive reservation must call
        ``assign_role_exclusive`` instead.
        """
        with self._state_lock:
            holder = self._claims.get(camera.device_path)
            if holder is not None:
                raise RoleConflict(
                    device_path=camera.device_path,
                    role=role,
                    current_holder=holder,
                    requested_by="<setup-wizard>",
                )
            prev = self._assignments.get(role)
            self._assignments[role] = camera
        if prev and prev.device_path != camera.device_path:
            log.info(
                "camera_role_reassigned",
                role=role.value,
                old=prev.name,
                new=camera.name,
            )
        else:
            log.info("camera_role_assigned", role=role.value, camera=camera.name)

    def assign_role_exclusive(
        self,
        camera: CameraInfo,
        role: CameraRole,
        plugin_id: str,
    ) -> None:
        """Exclusively claim a camera for a role on behalf of a plugin.

        The setup wizard cannot reassign or unassign the role until the
        plugin releases its claim via ``release_claim``. A subsequent
        call from the same ``plugin_id`` is idempotent (updates the
        role mapping but leaves the claim in place). A call from a
        different ``plugin_id`` raises :class:`RoleConflict`.

        Used by vision-navigation plugins (and similar) that need a
        stable camera reservation across wizard runs.
        """
        if not plugin_id:
            raise ValueError("plugin_id must be a non-empty string")
        with self._state_lock:
            holder = self._claims.get(camera.device_path)
            if holder is not None and holder != plugin_id:
                raise RoleConflict(
                    device_path=camera.device_path,
                    role=role,
                    current_holder=holder,
                    requested_by=plugin_id,
                )
            self._claims[camera.device_path] = plugin_id
            self._assignments[role] = camera
        log.info(
            "camera_role_assigned_exclusive",
            role=role.value,
            camera=camera.name,
            plugin_id=plugin_id,
        )

    def reassign_role_exclusive(
        self,
        camera: CameraInfo,
        role: CameraRole,
        plugin_id: str | None = None,
    ) -> str | None:
        """Force a role reassignment, dropping any existing exclusive claim.

        This is the explicit-override path. The operator has been
        warned (via 409 + confirm dialog on the GCS) that the camera is
        currently claimed by another plugin and has chosen to proceed.

        Behavior under the lock (single atomic operation so a concurrent
        ``assign_role_exclusive`` from the dropped plugin cannot wedge
        the state):

        * Drop any existing claim on ``camera.device_path``.
        * If ``plugin_id`` is a non-empty string, install a new claim
          mapping the device to ``plugin_id``.
        * If ``plugin_id`` is ``None`` or empty, leave the camera
          unclaimed (the setup wizard's NAV bind path uses this when
          the role is not held by a plugin).
        * Set the role assignment to ``camera``.

        Returns the dropped plugin id (or ``None`` if no claim existed)
        so the caller can log + surface it in the response.
        """
        normalized_id = plugin_id.strip() if isinstance(plugin_id, str) else None
        new_holder = normalized_id or None
        with self._state_lock:
            dropped = self._claims.pop(camera.device_path, None)
            if new_holder:
                self._claims[camera.device_path] = new_holder
            self._assignments[role] = camera
        log.warning(
            "camera_role_force_reassigned",
            role=role.value,
            camera=camera.name,
            device_path=camera.device_path,
            dropped_holder=dropped,
            new_holder=new_holder,
        )
        return dropped

    def release_claim(self, device_path: str, plugin_id: str) -> bool:
        """Release a plugin's exclusive claim on a camera.

        Returns True if a claim was released, False if there was none
        or the claim belonged to a different plugin (silent no-op for
        idempotency on plugin teardown). Roles assigned to the
        released camera stay in place; the wizard may overwrite them
        on the next assign.
        """
        with self._state_lock:
            holder = self._claims.get(device_path)
            if holder is None or holder != plugin_id:
                return False
            del self._claims[device_path]
        log.info(
            "camera_claim_released",
            device_path=device_path,
            plugin_id=plugin_id,
        )
        return True

    def claimed_by(self, device_path: str) -> str | None:
        """Return the plugin id that exclusively claims this camera,
        or None if the camera is shared."""
        with self._state_lock:
            return self._claims.get(device_path)

    def unassign_role(self, role: CameraRole) -> None:
        """Remove a role assignment."""
        removed = self._assignments.pop(role, None)
        if removed:
            log.info("camera_role_unassigned", role=role.value, camera=removed.name)

    def get_primary(self) -> CameraInfo | None:
        """Return the camera assigned as PRIMARY, or None."""
        return self._assignments.get(CameraRole.PRIMARY)

    def get_by_role(self, role: CameraRole) -> CameraInfo | None:
        """Return the camera assigned to a specific role, or None."""
        return self._assignments.get(role)

    def auto_assign(self) -> None:
        """Automatically assign roles based on camera type heuristics.

        Filters out non-camera hardware (codecs, ISPs, decoders).
        CSI cameras become PRIMARY (first) or SECONDARY (subsequent).
        USB cameras fill remaining roles.
        """
        # Filter out internal hardware devices (codecs, ISPs, decoders)
        real_cameras = [c for c in self._cameras if c.hardware_role == HardwareRole.CAMERA]
        filtered = len(self._cameras) - len(real_cameras)
        if filtered > 0:
            log.info("non_camera_devices_filtered", count=filtered)

        csi_cameras = [c for c in real_cameras if c.type == CameraType.CSI]
        usb_cameras = [c for c in real_cameras if c.type == CameraType.USB]
        ip_cameras = [c for c in real_cameras if c.type == CameraType.IP]

        all_ordered = csi_cameras + usb_cameras + ip_cameras

        if len(all_ordered) >= 1:
            self.assign_role(all_ordered[0], CameraRole.PRIMARY)
        if len(all_ordered) >= 2:
            self.assign_role(all_ordered[1], CameraRole.SECONDARY)

        log.info(
            "camera_auto_assign_complete",
            total=len(all_ordered),
            assigned=len(self._assignments),
        )

    def to_dict(self) -> dict:
        """Serialize state for API responses.

        Each camera entry is enriched with a ``claimed_by`` field so
        the GCS can render an "in use by <plugin>" badge and disable
        wizard reassignment for exclusively-claimed cameras.
        """
        with self._state_lock:
            claims_snapshot = dict(self._claims)
            assignments_snapshot = dict(self._assignments)
            cameras_snapshot = list(self._cameras)

        def _cam_with_claim(cam: CameraInfo) -> dict:
            d = cam.to_dict()
            d["claimed_by"] = claims_snapshot.get(cam.device_path)
            return d

        return {
            "cameras": [_cam_with_claim(c) for c in cameras_snapshot],
            "assignments": {
                role.value: _cam_with_claim(cam)
                for role, cam in assignments_snapshot.items()
            },
        }
