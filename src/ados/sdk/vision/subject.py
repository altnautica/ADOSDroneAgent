"""The designated subject's world state (``vision.subject.v1``).

The plugin host republishes the vision engine's subject estimate as the
host-only event topic :data:`VISION_SUBJECT_TOPIC`; a plugin reads it with
``event.subscribe`` plus ``vision.subject.read``. Each event payload is one
msgpack map, decoded here into :class:`SubjectState`. Positions and
velocities are local NED metres; ``pos_ned`` is the subject's ground-contact
point; covariances are row-major 3 x 3 in m². The wire contract is
``ados_protocol::vision_subject`` in the agent.
"""

from __future__ import annotations

import math
from dataclasses import dataclass
from typing import Any

VISION_SUBJECT_TOPIC = "vision.subject.v1"
"""The host event topic the subject state is published on."""

VISION_SUBJECT_READ_CAP = "vision.subject.read"
"""The capability (beside ``event.subscribe``) that reads the topic."""

VISION_SUBJECT_VERSION = 1
"""The record version this decoder reads."""

MODE_NAMES = ("cv", "ct", "stop")
"""Motion models behind ``mode_probs``: constant velocity, coordinated turn,
standing still."""

SOURCE_NAMES = ("box", "depth", "ground", "beacon")
"""Cues that can feed an estimate."""


def _floats(raw: Any, n: int, field: str) -> tuple[float, ...]:
    if not isinstance(raw, (list, tuple)) or len(raw) != n:
        raise ValueError(f"subject field {field}: not {n} numbers")
    out = tuple(float(x) for x in raw)
    if not all(math.isfinite(x) for x in out):
        raise ValueError(f"subject field {field}: not finite")
    return out


def _uint(payload: dict[str, Any], field: str) -> int:
    raw = payload.get(field)
    if not isinstance(raw, int) or isinstance(raw, bool) or raw < 0:
        raise ValueError(f"subject field {field}: not an unsigned integer")
    return raw


@dataclass(frozen=True)
class SubjectPrediction:
    """The expected position ``dt_s`` seconds after the state's stamp."""

    dt_s: float
    pos_ned: tuple[float, float, float]
    cov: tuple[float, ...]


@dataclass(frozen=True)
class SubjectState:
    """The designated subject's state at one instant."""

    camera_id: str
    id: int
    """The subject's published ``track_id`` on the detection stream."""
    stamp_ns: int
    fix_age_ms: int
    """Time since the last measurement of any kind; 0 when this update fixed."""
    pos_ned: tuple[float, float, float]
    cov: tuple[float, ...]
    vel_ned: tuple[float, float, float]
    mode_probs: tuple[float, float, float]
    height_m: float
    predictions: tuple[SubjectPrediction, ...]
    sources: frozenset[str]

    @classmethod
    def from_payload(cls, payload: dict[str, Any]) -> SubjectState:
        """Decode one state payload; :class:`ValueError` when it is not a
        ``vision.subject.v1`` state (a released record included: read those
        with :func:`decode_subject_record`)."""
        if not isinstance(payload, dict):
            raise ValueError("subject state is not a map")
        if "released" in payload:
            raise ValueError("subject record is a release, not a state")
        version = _uint(payload, "v")
        if version != VISION_SUBJECT_VERSION:
            raise ValueError(f"subject state version {version} is not {VISION_SUBJECT_VERSION}")
        camera_id = payload.get("camera_id")
        if not isinstance(camera_id, str):
            raise ValueError("subject field camera_id: not a string")
        preds_raw = payload.get("predictions")
        if not isinstance(preds_raw, list):
            raise ValueError("subject field predictions: not a list")
        predictions = []
        for p in preds_raw:
            if not isinstance(p, dict):
                raise ValueError("subject field predictions: entry not a map")
            pos = _floats(p.get("pos_ned"), 3, "predictions.pos_ned")
            predictions.append(
                SubjectPrediction(
                    dt_s=_floats([p.get("dt_s")], 1, "predictions.dt_s")[0],
                    pos_ned=(pos[0], pos[1], pos[2]),
                    cov=_floats(p.get("cov"), 9, "predictions.cov"),
                )
            )
        sources_raw = payload.get("sources")
        if not isinstance(sources_raw, list) or not all(
            isinstance(s, str) for s in sources_raw
        ):
            raise ValueError("subject field sources: not a list of names")
        pos = _floats(payload.get("pos_ned"), 3, "pos_ned")
        vel = _floats(payload.get("vel_ned"), 3, "vel_ned")
        modes = _floats(payload.get("mode_probs"), 3, "mode_probs")
        return cls(
            camera_id=camera_id,
            id=_uint(payload, "id"),
            stamp_ns=_uint(payload, "stamp_ns"),
            fix_age_ms=_uint(payload, "fix_age_ms"),
            pos_ned=(pos[0], pos[1], pos[2]),
            cov=_floats(payload.get("cov"), 9, "cov"),
            vel_ned=(vel[0], vel[1], vel[2]),
            mode_probs=(modes[0], modes[1], modes[2]),
            height_m=_floats([payload.get("height_m")], 1, "height_m")[0],
            predictions=tuple(predictions),
            sources=frozenset(sources_raw),
        )


@dataclass(frozen=True)
class SubjectReleased:
    """The designated subject is gone: the designation ended or changed, or
    the estimate coasted past its limit. Drop the subject on it."""

    camera_id: str
    id: int
    """The released subject's ``track_id``."""
    stamp_ns: int


def decode_subject_record(payload: dict[str, Any]) -> SubjectState | SubjectReleased:
    """Decode one ``vision.subject.v1`` event payload, a state or a release;
    :class:`ValueError` when it is neither."""
    if not isinstance(payload, dict):
        raise ValueError("subject record is not a map")
    if "released" not in payload:
        return SubjectState.from_payload(payload)
    if payload.get("released") is not True:
        raise ValueError("subject field released: not true")
    version = _uint(payload, "v")
    if version != VISION_SUBJECT_VERSION:
        raise ValueError(f"subject record version {version} is not {VISION_SUBJECT_VERSION}")
    camera_id = payload.get("camera_id")
    if not isinstance(camera_id, str):
        raise ValueError("subject field camera_id: not a string")
    return SubjectReleased(
        camera_id=camera_id,
        id=_uint(payload, "id"),
        stamp_ns=_uint(payload, "stamp_ns"),
    )
