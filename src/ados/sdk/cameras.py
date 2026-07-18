"""Camera-binding resolver for plugins (the camera-selector control).

A plugin that needs a camera (a follow behaviour reading detections, a gimbal
tracking a leg, a thermal reader) declares a ``camera-selector`` parameter
instead of hard-coding a device string. The operator either pins a specific
camera from the node's roster (explicit) or leaves it on auto, in which case the
agent resolves the first available camera matching the plugin's declared
requirement (a purpose such as ``detect``, an orientation such as ``down``),
falling back to the primary stream. This module is the resolver both sides use.

The declarative parameter shape (a free-form entry in
``gcs.contributes.parameters``) is::

    {
      "key": "camera",
      "control": "camera-selector",     # this control type
      "binding": "agent.config",        # the value lands in the plugin's config
      "label": "Camera",
      "purpose": "detect",              # optional required-purpose filter
      "orientation": "down",            # optional required-orientation filter
      "default": "auto"                 # "auto" = by-requirement; an id = explicit
    }

The stored value the plugin reads from its config is either the sentinel
``"auto"`` (or an empty value — by-requirement) or a concrete camera id
(explicit). :func:`resolve_camera_selection` turns that value, plus the camera
roster (the rows from ``GET /api/video/roster``), into the concrete leg id the
plugin binds to — or ``None`` when no usable camera is available, so a safety
plugin stops rather than binding to a dead device.
"""

from __future__ import annotations

from typing import Any

# The parameter control type a plugin declares for a camera binding.
CAMERA_SELECTOR_CONTROL = "camera-selector"

# The binding a camera-selector uses: the value lands in the plugin's agent
# config, read live each loop.
CAMERA_SELECTOR_BINDING = "agent.config"

# The sentinel value meaning "resolve by requirement" (the by-requirement mode).
# An empty value / ``None`` is treated the same way.
CAMERA_SELECTOR_AUTO = "auto"


def _is_by_requirement(selection: Any) -> bool:
    """True when the selection asks for by-requirement resolution: the auto
    sentinel, an empty string, or ``None``."""
    if selection is None:
        return True
    if isinstance(selection, str):
        stripped = selection.strip()
        return stripped == "" or stripped == CAMERA_SELECTOR_AUTO
    return False


def _available(camera: dict) -> bool:
    """True when a camera is usable for a binding: enabled by the operator and
    not offline (its device is present, or it is a network leg). A roster row
    that omits the fields (a hand-built list) defaults to available."""
    enabled = camera.get("enabled", True)
    state = camera.get("state", "assigned")
    return bool(enabled) and state != "offline"


def _matches(camera: dict, purpose: str | None, orientation: str | None) -> bool:
    """True when a camera satisfies the required purpose AND orientation (each
    only checked when the requirement is set)."""
    if purpose:
        purposes = camera.get("purpose") or []
        if purpose not in purposes:
            return False
    if orientation:
        if camera.get("orientation") != orientation:
            return False
    return True


def _camera_id(camera: dict) -> str | None:
    """The camera's id, or ``None`` when the row carries no usable id."""
    cid = camera.get("id")
    if isinstance(cid, str) and cid:
        return cid
    return None


def primary_camera_id(cameras: list[dict]) -> str | None:
    """The id of the primary camera — the leg with role ``primary``, else the
    first available leg, else the first leg — matching the video pipeline's
    primary resolution. ``None`` for an empty roster."""
    for cam in cameras:
        if cam.get("role") == "primary" and _available(cam):
            cid = _camera_id(cam)
            if cid:
                return cid
    for cam in cameras:
        if _available(cam):
            cid = _camera_id(cam)
            if cid:
                return cid
    for cam in cameras:
        cid = _camera_id(cam)
        if cid:
            return cid
    return None


def resolve_camera_selection(
    selection: Any,
    cameras: list[dict],
    *,
    purpose: str | None = None,
    orientation: str | None = None,
) -> str | None:
    """Resolve a camera-selector value against the roster to a concrete leg id.

    * **Explicit** (``selection`` is a concrete camera id): returns that id when
      a camera with it exists AND is available (enabled + present); otherwise
      ``None`` — the operator pinned a camera that is not usable, so the plugin
      surfaces "no camera" rather than silently binding to a different one.
    * **By-requirement** (``selection`` is ``None`` / empty / ``"auto"``):
      returns the first available camera matching the required ``purpose`` AND
      ``orientation`` (each applied only when set); when none matches, falls back
      to the primary camera; when the roster is empty, ``None``.

    ``cameras`` is the roster (the rows from ``GET /api/video/roster``), or any
    list of camera dicts carrying at least an ``id`` (and optionally ``enabled``
    / ``state`` / ``purpose`` / ``orientation`` / ``role``).
    """
    if not _is_by_requirement(selection):
        target = str(selection).strip()
        for cam in cameras:
            if _camera_id(cam) == target and _available(cam):
                return target
        return None

    # By-requirement: first available leg matching the requirement.
    for cam in cameras:
        if _available(cam) and _matches(cam, purpose, orientation):
            cid = _camera_id(cam)
            if cid:
                return cid
    # No requirement match → the primary stream.
    return primary_camera_id(cameras)


__all__ = [
    "CAMERA_SELECTOR_CONTROL",
    "CAMERA_SELECTOR_BINDING",
    "CAMERA_SELECTOR_AUTO",
    "primary_camera_id",
    "resolve_camera_selection",
]
