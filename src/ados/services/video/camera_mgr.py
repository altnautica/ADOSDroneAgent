"""Camera manager — maps discovered cameras to roles for the video pipeline."""

from __future__ import annotations

from enum import StrEnum

from ados.core.logging import get_logger
from ados.hal.camera import CameraInfo, CameraType

log = get_logger("video.camera_mgr")


class CameraRole(StrEnum):
    PRIMARY = "primary"
    SECONDARY = "secondary"
    THERMAL = "thermal"
    INSPECTION = "inspection"


class CameraManager:
    """Manages camera-to-role assignment.

    Cameras are discovered by the HAL layer and then assigned roles here.  The
    pipeline uses the role assignment to decide which camera feeds which stream.
    """

    def __init__(self) -> None:
        self._assignments: dict[CameraRole, CameraInfo] = {}
        self._cameras: list[CameraInfo] = []

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
        """
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

        CSI cameras become PRIMARY (first) or SECONDARY (subsequent).
        USB cameras fill remaining roles.
        """
        csi_cameras = [c for c in self._cameras if c.type == CameraType.CSI]
        usb_cameras = [c for c in self._cameras if c.type == CameraType.USB]
        ip_cameras = [c for c in self._cameras if c.type == CameraType.IP]

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
        """Serialize state for API responses."""
        return {
            "cameras": [c.to_dict() for c in self._cameras],
            "assignments": {
                role.value: cam.to_dict() for role, cam in self._assignments.items()
            },
        }
