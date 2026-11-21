"""The ``vision.subject.v1`` decoder and subscription in the Python SDK."""

from __future__ import annotations

from typing import Any

import pytest

from ados.sdk.vision import (
    VISION_SUBJECT_TOPIC,
    SubjectReleased,
    SubjectState,
    VisionClient,
    decode_subject_record,
)


def _payload(**over: Any) -> dict[str, Any]:
    """A record with the field names and shapes the Rust encoder writes."""
    p: dict[str, Any] = {
        "v": 1,
        "camera_id": "main",
        "id": 7,
        "stamp_ns": 1_000_000_000,
        "fix_age_ms": 0,
        "pos_ned": [8.0, -2.5, 0.0],
        "cov": [0.2, 0.0, 0.0, 0.0, 0.2, 0.0, 0.0, 0.0, 0.05],
        "vel_ned": [1.4, 0.0, 0.0],
        "mode_probs": [0.7, 0.2, 0.1],
        "height_m": 1.74,
        "predictions": [
            {"dt_s": dt, "pos_ned": [8.0 + 1.4 * dt, -2.5, 0.0], "cov": [dt] * 9}
            for dt in (0.5, 1.0, 1.5, 2.0, 3.0)
        ],
        "sources": ["box", "beacon"],
    }
    p.update(over)
    return p


def test_a_record_decodes() -> None:
    s = SubjectState.from_payload(_payload())
    assert (s.camera_id, s.id, s.stamp_ns, s.fix_age_ms) == ("main", 7, 1_000_000_000, 0)
    assert s.pos_ned == (8.0, -2.5, 0.0)
    assert s.vel_ned == (1.4, 0.0, 0.0)
    assert len(s.cov) == 9
    assert [p.dt_s for p in s.predictions] == [0.5, 1.0, 1.5, 2.0, 3.0]
    assert s.predictions[-1].pos_ned == pytest.approx((12.2, -2.5, 0.0))
    assert s.sources == frozenset({"box", "beacon"})


@pytest.mark.parametrize(
    "over",
    [
        {"v": 2},
        {"pos_ned": [1.0, 2.0]},
        {"cov": [0.0] * 8},
        {"height_m": float("nan")},
        {"predictions": [{"dt_s": 0.5}]},
        {"sources": "box"},
        {"id": -1},
    ],
)
def test_a_malformed_record_is_refused(over: dict[str, Any]) -> None:
    with pytest.raises(ValueError):
        SubjectState.from_payload(_payload(**over))


_RELEASED = {"v": 1, "camera_id": "main", "id": 7, "stamp_ns": 2_000_000_000, "released": True}


def test_a_released_record_decodes_and_is_not_a_state() -> None:
    assert decode_subject_record(_RELEASED) == SubjectReleased("main", 7, 2_000_000_000)
    assert isinstance(decode_subject_record(_payload()), SubjectState)
    with pytest.raises(ValueError):
        SubjectState.from_payload(_RELEASED)
    with pytest.raises(ValueError):
        decode_subject_record({**_RELEASED, "released": False})


class _Ipc:
    def __init__(self) -> None:
        self.subs: dict[str, Any] = {}

    async def event_subscribe(self, topic: str, cb: Any) -> None:
        self.subs[topic] = cb


async def test_subscribe_subject_decodes_and_filters_by_camera() -> None:
    ipc = _Ipc()
    got: list[SubjectState] = []
    await VisionClient(ipc).subscribe_subject(got.append, camera_id="main")  # type: ignore[arg-type]
    cb = ipc.subs[VISION_SUBJECT_TOPIC]
    await cb(_payload())
    await cb(_payload(camera_id="aux"))
    await cb({"v": 1})
    assert [s.camera_id for s in got] == ["main"]


async def test_subscribe_subject_delivers_the_release() -> None:
    ipc = _Ipc()
    got: list[SubjectState | SubjectReleased] = []
    await VisionClient(ipc).subscribe_subject(got.append)  # type: ignore[arg-type]
    cb = ipc.subs[VISION_SUBJECT_TOPIC]
    await cb(_payload())
    await cb(_RELEASED)
    assert [type(r).__name__ for r in got] == ["SubjectState", "SubjectReleased"]
