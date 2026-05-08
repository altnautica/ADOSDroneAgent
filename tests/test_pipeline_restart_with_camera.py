"""Tests for ``VideoPipeline.restart_with_camera``.

Three concerns:

1. Role assignment + encoder restart fires for a known camera.
2. The structlog ``pipeline_camera_switched`` event is emitted with both
   the previous and new device paths.
3. Switching while recording rotates the file: two real MP4 files end up
   on disk, one before and one after the switch.

The encoder + mediamtx subprocesses are mocked because they need
hardware. The recorder is left mostly real so the file rotation can be
observed on the filesystem; only the ffmpeg subprocess it spawns is
replaced with ``sh`` writing bytes to the target path so we get real
files without needing ffmpeg installed.
"""

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock, MagicMock

import pytest

from ados.core.config import VideoConfig
from ados.hal.camera import CameraInfo, CameraType
from ados.services.video.pipeline import VideoPipeline

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_cameras() -> list[CameraInfo]:
    return [
        CameraInfo(
            name="CSI-0",
            type=CameraType.CSI,
            device_path="/dev/video0",
            width=1920,
            height=1080,
        ),
        CameraInfo(
            name="USB",
            type=CameraType.USB,
            device_path="/dev/video2",
            width=1280,
            height=720,
        ),
    ]


@pytest.fixture
def pipeline(tmp_path: Path) -> VideoPipeline:
    cfg = VideoConfig()
    cfg.recording.path = str(tmp_path)
    pipe = VideoPipeline(cfg)
    pipe.camera_manager.set_cameras(_make_cameras())
    pipe.camera_manager.auto_assign()
    return pipe


# ---------------------------------------------------------------------------
# Basic restart_with_camera contract
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_restart_with_camera_assigns_role(
    pipeline: VideoPipeline, monkeypatch: pytest.MonkeyPatch
) -> None:
    # No-op the encoder/mediamtx interactions.
    monkeypatch.setattr(pipeline, "stop_stream", AsyncMock())
    monkeypatch.setattr(
        pipeline, "_restart_after_assign", AsyncMock(return_value=True)
    )

    assert pipeline.camera_manager.get_primary().device_path == "/dev/video0"

    await pipeline.restart_with_camera("primary", "/dev/video2")

    assert pipeline.camera_manager.get_primary().device_path == "/dev/video2"


@pytest.mark.asyncio
async def test_restart_with_camera_unknown_device_raises(
    pipeline: VideoPipeline,
) -> None:
    with pytest.raises(LookupError):
        await pipeline.restart_with_camera("primary", "/dev/video99")


@pytest.mark.asyncio
async def test_restart_with_camera_unknown_role_raises(
    pipeline: VideoPipeline,
) -> None:
    with pytest.raises(ValueError):
        await pipeline.restart_with_camera("thermal-x", "/dev/video0")


@pytest.mark.asyncio
async def test_restart_with_camera_emits_struct_event(
    pipeline: VideoPipeline, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(pipeline, "stop_stream", AsyncMock())
    monkeypatch.setattr(
        pipeline, "_restart_after_assign", AsyncMock(return_value=True)
    )

    events: list[tuple[str, dict[str, Any]]] = []

    def _capture(name: str, **kwargs: Any) -> None:
        events.append((name, kwargs))

    fake_log = MagicMock()
    fake_log.info.side_effect = _capture
    fake_log.warning.side_effect = _capture
    fake_log.error.side_effect = _capture
    monkeypatch.setattr("ados.services.video.pipeline.log", fake_log)

    await pipeline.restart_with_camera("primary", "/dev/video2")

    # Final event must be the canonical camera-switched event with both
    # device paths.
    switched = [e for e in events if e[0] == "pipeline_camera_switched"]
    assert switched, "pipeline_camera_switched event missing"
    name, kwargs = switched[-1]
    assert kwargs["from_device_path"] == "/dev/video0"
    assert kwargs["to_device_path"] == "/dev/video2"
    assert kwargs["role"] == "primary"


# ---------------------------------------------------------------------------
# Concurrent calls serialize on the lock
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_restart_with_camera_serialized(
    pipeline: VideoPipeline, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Two concurrent switches finish in arrival order, never overlapping."""
    in_flight = 0
    max_in_flight = 0
    order: list[str] = []

    async def _fake_stop_stream() -> None:
        nonlocal in_flight, max_in_flight
        in_flight += 1
        max_in_flight = max(max_in_flight, in_flight)
        await asyncio.sleep(0.05)
        in_flight -= 1

    async def _fake_restart() -> bool:
        return True

    monkeypatch.setattr(pipeline, "stop_stream", _fake_stop_stream)
    monkeypatch.setattr(pipeline, "_restart_after_assign", _fake_restart)

    async def _switch(path: str) -> None:
        await pipeline.restart_with_camera("primary", path)
        order.append(path)

    await asyncio.gather(
        _switch("/dev/video2"),
        _switch("/dev/video0"),
    )

    assert max_in_flight == 1, "switches overlapped despite the lock"
    assert order == ["/dev/video2", "/dev/video0"]


# ---------------------------------------------------------------------------
# Switch while recording rotates the on-disk MP4 file
# ---------------------------------------------------------------------------


class _FakeFfmpegProcess:
    """Minimal stand-in for an ``asyncio.subprocess.Process`` running ffmpeg.

    Writes a sentinel byte to the output path on construction so the
    recorder sees a real file land on disk. Honours the stdin ``q``
    sentinel that VideoRecorder.stop_recording() sends.
    """

    def __init__(self, output_path: Path) -> None:
        # Ensure the output file appears immediately so list_recordings()
        # can observe the boundary file even if stop is called fast.
        output_path.parent.mkdir(parents=True, exist_ok=True)
        # Sentinel content per recording — the recorder doesn't validate
        # MP4 headers, only that the file exists.
        output_path.write_bytes(b"\x00\x00\x00\x18ftypmp42\x00")
        self.pid = 999_999
        self.returncode: int | None = None
        self.stdin = MagicMock()
        self.stdin.write = MagicMock()
        self.stdin.drain = AsyncMock()

    async def wait(self) -> int:
        if self.returncode is None:
            self.returncode = 0
        return self.returncode

    def kill(self) -> None:
        self.returncode = -9

    def terminate(self) -> None:
        self.returncode = 0


@pytest.mark.asyncio
async def test_restart_with_camera_rotates_recording_file(
    pipeline: VideoPipeline,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Switch while recording → two real MP4 files on disk.

    REAL test: we patch only ``asyncio.create_subprocess_exec`` inside
    ``ados.services.video.recorder``, which causes the recorder to write
    a real file to ``tmp_path`` for each ``start_recording`` call.
    Encoder + mediamtx are mocked because they need hardware.
    """
    spawned: list[_FakeFfmpegProcess] = []

    async def _fake_recorder_spawn(
        *args: Any, **kwargs: Any
    ) -> _FakeFfmpegProcess:
        # The recorder builds the ffmpeg cmd as: ['ffmpeg', '-y', '-i',
        # source, '-c', 'copy', '-movflags', '+faststart', filepath].
        # Last positional is the output path.
        output = Path(args[-1])
        proc = _FakeFfmpegProcess(output)
        spawned.append(proc)
        return proc

    monkeypatch.setattr(
        "ados.services.video.recorder.asyncio.create_subprocess_exec",
        _fake_recorder_spawn,
    )

    # Encoder + mediamtx are not under test here.
    monkeypatch.setattr(pipeline, "stop_stream", AsyncMock())
    monkeypatch.setattr(
        pipeline, "_restart_after_assign", AsyncMock(return_value=True)
    )

    # Begin a recording on the original camera. The fake spawn writes
    # the first MP4 file to disk.
    first_path = await pipeline.recorder.start_recording()
    assert first_path, "first recording did not start"
    assert Path(first_path).is_file()
    assert pipeline.recorder.recording is True

    # Switch primary to /dev/video2 while recording is active. The
    # pipeline must stop the in-flight recorder, restart the encoder,
    # and resume recording into a fresh file.
    await pipeline.restart_with_camera("primary", "/dev/video2")

    # The recorder is back on after the switch and pointing at a new file.
    assert pipeline.recorder.recording is True
    second_path = pipeline.recorder.current_path
    assert second_path
    assert second_path != first_path
    assert Path(second_path).is_file()

    # Both files exist on disk simultaneously — file rotation is real.
    on_disk = sorted(p.name for p in tmp_path.glob("*.mp4"))
    assert len(on_disk) == 2, f"expected 2 mp4 files, got {on_disk}"

    # Two distinct ffmpeg spawns: one before the switch, one after.
    assert len(spawned) == 2

    # Cleanup: stop the active recording so the watcher tasks settle.
    await pipeline.recorder.stop_recording()


@pytest.mark.asyncio
async def test_restart_with_camera_no_recording_no_rotation(
    pipeline: VideoPipeline,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Switch when idle: no extra files appear on disk."""
    monkeypatch.setattr(pipeline, "stop_stream", AsyncMock())
    monkeypatch.setattr(
        pipeline, "_restart_after_assign", AsyncMock(return_value=True)
    )

    assert pipeline.recorder.recording is False
    await pipeline.restart_with_camera("primary", "/dev/video2")

    assert pipeline.recorder.recording is False
    on_disk = list(tmp_path.glob("*.mp4"))
    assert on_disk == []
