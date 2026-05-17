"""Tests for the video pipeline service."""

from __future__ import annotations

import os
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from ados.core.config import VideoConfig
from ados.services.video.pipeline import PipelineState, VideoPipeline


class TestPipelineState:
    def test_enum_values(self):
        assert PipelineState.STOPPED == "stopped"
        assert PipelineState.STARTING == "starting"
        assert PipelineState.RUNNING == "running"
        assert PipelineState.ERROR == "error"


class TestVideoPipeline:
    def test_initial_state(self):
        config = VideoConfig()
        pipeline = VideoPipeline(config)
        assert pipeline.state == PipelineState.STOPPED
        assert pipeline.camera_manager is not None
        assert pipeline.recorder is not None
        assert pipeline.mediamtx is not None

    def test_get_status_when_stopped(self):
        config = VideoConfig()
        pipeline = VideoPipeline(config)
        status = pipeline.get_status()
        assert status["state"] == "stopped"
        assert status["encoder"] is None
        assert "cameras" in status
        assert "recorder" in status
        assert "mediamtx" in status

    @pytest.mark.asyncio
    async def test_start_stream_no_cameras(self):
        config = VideoConfig()
        pipeline = VideoPipeline(config)

        with patch("ados.services.video.pipeline.discover_cameras", return_value=[]):
            result = await pipeline.start_stream()
            assert result is False
            assert pipeline.state == PipelineState.ERROR

    @pytest.mark.asyncio
    async def test_start_stream_no_encoder(self):
        from ados.hal.camera import CameraInfo, CameraType

        cam = CameraInfo(name="Test", type=CameraType.CSI, device_path="/dev/video0")
        config = VideoConfig()
        pipeline = VideoPipeline(config)

        with (
            patch("ados.services.video.pipeline.discover_cameras", return_value=[cam]),
            patch("ados.services.video.pipeline.detect_encoder_for_camera", return_value=None),
        ):
            result = await pipeline.start_stream()
            assert result is False
            assert pipeline.state == PipelineState.ERROR

    @pytest.mark.asyncio
    async def test_stop_stream_when_not_running(self):
        config = VideoConfig()
        pipeline = VideoPipeline(config)
        await pipeline.stop_stream()
        assert pipeline.state == PipelineState.STOPPED

    @pytest.mark.asyncio
    async def test_check_health_no_process(self):
        config = VideoConfig()
        pipeline = VideoPipeline(config)
        assert await pipeline._check_health() is False

    @pytest.mark.asyncio
    async def test_check_health_process_exited(self):
        config = VideoConfig()
        pipeline = VideoPipeline(config)
        mock_proc = MagicMock()
        mock_proc.returncode = 1
        pipeline._encoder_process = mock_proc
        assert await pipeline._check_health() is False

    @pytest.mark.asyncio
    async def test_check_health_process_running(self):
        config = VideoConfig()
        pipeline = VideoPipeline(config)
        mock_proc = MagicMock()
        mock_proc.returncode = None
        # Use the test process pid so os.kill(pid, 0) succeeds
        mock_proc.pid = os.getpid()
        pipeline._encoder_process = mock_proc
        # Pretend mediamtx is alive and within startup grace period
        pipeline._mediamtx = MagicMock()
        pipeline._mediamtx.is_running = MagicMock(return_value=True)
        import time as _time
        pipeline._started_at = _time.monotonic()
        assert await pipeline._check_health() is True

    @pytest.mark.asyncio
    async def test_start_stream_already_running(self):
        config = VideoConfig()
        pipeline = VideoPipeline(config)
        pipeline._state = PipelineState.RUNNING
        result = await pipeline.start_stream()
        assert result is True


def _pipeline_with_mediamtx_mocks():
    """Pipeline with camera + mediamtx fully mocked. Used for partial-start tests."""
    from ados.hal.camera import CameraInfo, CameraType

    config = VideoConfig()
    pipeline = VideoPipeline(config)
    cam = CameraInfo(name="Test", type=CameraType.CSI, device_path="/dev/video0")
    pipeline._discover_and_assign = MagicMock(
        side_effect=lambda: pipeline._camera_mgr.set_cameras([cam])
    )
    pipeline._camera_mgr.set_cameras = MagicMock()
    pipeline._camera_mgr.get_primary = MagicMock(return_value=cam)
    # Replace the manager wholesale so we can mock the read-only property too
    mtx = MagicMock()
    mtx.generate_config = MagicMock()
    mtx.start = AsyncMock(return_value=True)
    mtx.stop = AsyncMock(return_value=None)
    mtx.rtsp_port = 8554
    pipeline._mediamtx = mtx
    return pipeline


class TestPipelinePartialStartCleanup:
    """If post-mediamtx setup fails, mediamtx must be torn down so the next
    start_stream() does not collide on the port with a zombie."""

    @pytest.mark.asyncio
    async def test_filenotfound_during_encoder_spawn_stops_mediamtx(self, monkeypatch):
        from ados.services.video.encoder import EncoderType

        pipeline = _pipeline_with_mediamtx_mocks()
        monkeypatch.setattr(
            "ados.services.video.pipeline.detect_encoder_for_camera",
            lambda cam: EncoderType.FFMPEG,
        )
        monkeypatch.setattr(
            "ados.services.video.pipeline.build_encoder_command",
            lambda *a, **k: ["ffmpeg", "-i", "x", "y"],
        )

        async def boom(*args, **kwargs):
            raise FileNotFoundError("ffmpeg")

        import asyncio as _asyncio
        monkeypatch.setattr(_asyncio, "create_subprocess_exec", boom)

        ok = await pipeline.start_stream()
        assert ok is False
        assert pipeline.state == PipelineState.ERROR
        pipeline._mediamtx.stop.assert_awaited_once()

    @pytest.mark.asyncio
    async def test_oserror_during_encoder_spawn_stops_mediamtx(self, monkeypatch):
        from ados.services.video.encoder import EncoderType

        pipeline = _pipeline_with_mediamtx_mocks()
        monkeypatch.setattr(
            "ados.services.video.pipeline.detect_encoder_for_camera",
            lambda cam: EncoderType.FFMPEG,
        )
        monkeypatch.setattr(
            "ados.services.video.pipeline.build_encoder_command",
            lambda *a, **k: ["ffmpeg", "-i", "x", "y"],
        )

        # start_stream() first calls _kill_orphan_encoder_ffmpegs and
        # _kill_orphan_bridge_ffmpegs, each of which spawns ``pgrep``
        # subprocesses via create_subprocess_exec. The OSError must fire
        # only on the encoder ffmpeg spawn, not on the pgrep sweeps.
        # Discriminate by argv[0]: pgrep returns an empty-stdout mock,
        # ffmpeg raises OSError.
        async def _fake_exec(*args, **kwargs):
            if args and args[0] == "pgrep":
                proc = MagicMock()
                proc.communicate = AsyncMock(return_value=(b"", b""))
                return proc
            raise OSError("permission denied")

        import asyncio as _asyncio
        monkeypatch.setattr(_asyncio, "create_subprocess_exec", _fake_exec)

        ok = await pipeline.start_stream()
        assert ok is False
        assert pipeline.state == PipelineState.ERROR
        pipeline._mediamtx.stop.assert_awaited_once()

    @pytest.mark.asyncio
    async def test_mediamtx_failed_to_start_does_not_call_stop(self, monkeypatch):
        from ados.services.video.encoder import EncoderType

        pipeline = _pipeline_with_mediamtx_mocks()
        pipeline._mediamtx.start = AsyncMock(return_value=False)
        monkeypatch.setattr(
            "ados.services.video.pipeline.detect_encoder_for_camera",
            lambda cam: EncoderType.FFMPEG,
        )
        monkeypatch.setattr(
            "ados.services.video.pipeline.build_encoder_command",
            lambda *a, **k: ["ffmpeg", "-i", "x", "y"],
        )

        ok = await pipeline.start_stream()
        assert ok is False
        assert pipeline.state == PipelineState.ERROR
        pipeline._mediamtx.stop.assert_not_awaited()


class TestRestartAttemptsSurface:
    """Public restart counter + healthy-window reset behaviour."""

    def test_restart_attempts_starts_at_zero(self):
        pipeline = VideoPipeline(VideoConfig())
        assert pipeline.restart_attempts() == 0

    def test_restart_attempts_reflects_internal_counter(self):
        pipeline = VideoPipeline(VideoConfig())
        pipeline._restart_count = 4
        assert pipeline.restart_attempts() == 4

    def test_first_healthy_tick_seeds_timer_without_clearing(self):
        pipeline = VideoPipeline(VideoConfig())
        pipeline._restart_count = 3
        cleared = pipeline._note_healthy_tick(now=100.0)
        # First call records the stamp but does not yet clear because
        # we have not measured a window of healthy time.
        assert cleared is False
        assert pipeline._last_healthy_at == 100.0
        assert pipeline._restart_count == 3

    def test_sustained_healthy_window_clears_counter(self):
        pipeline = VideoPipeline(VideoConfig())
        pipeline._restart_count = 5
        pipeline._note_healthy_tick(now=100.0)
        # 30 s in: still inside the window, counter intact.
        assert pipeline._note_healthy_tick(now=130.0) is False
        assert pipeline._restart_count == 5
        # 65 s in: past the 60 s window, counter clears.
        assert pipeline._note_healthy_tick(now=165.0) is True
        assert pipeline._restart_count == 0

    def test_no_clear_when_counter_already_zero(self):
        pipeline = VideoPipeline(VideoConfig())
        pipeline._note_healthy_tick(now=100.0)
        # Past the window with a clean counter — nothing to clear.
        assert pipeline._note_healthy_tick(now=200.0) is False
        assert pipeline._restart_count == 0

    def test_unhealthy_tick_resets_window(self):
        pipeline = VideoPipeline(VideoConfig())
        pipeline._restart_count = 4
        pipeline._note_healthy_tick(now=100.0)
        # Failure interrupts the streak.
        pipeline._note_unhealthy_tick()
        assert pipeline._last_healthy_at == 0.0
        # Counter has not been touched by the unhealthy tick itself.
        assert pipeline._restart_count == 4
        # New healthy stamp at t=200; another window must elapse before
        # the counter can clear (test that we did not retain the old
        # 100.0 stamp somehow).
        pipeline._note_healthy_tick(now=200.0)
        assert pipeline._last_healthy_at == 200.0
        # 30 s later, still within window — no clear.
        assert pipeline._note_healthy_tick(now=230.0) is False
        assert pipeline._restart_count == 4
        # 65 s after the new stamp, clear fires.
        assert pipeline._note_healthy_tick(now=270.0) is True
        assert pipeline._restart_count == 0


class TestWfbUdpTee:
    """The wfb tee feeds the radio's UDP listener so video crosses the
    radio link. Without this, wfb_tx starves and zero bytes hit the GS."""

    @pytest.mark.asyncio
    async def test_start_wfb_tee_spawns_ffmpeg_with_correct_args(self, monkeypatch):
        from ados.services.video import pipeline as pl_mod

        pipeline = VideoPipeline(VideoConfig())
        pipeline._state = PipelineState.RUNNING
        # Mediamtx port lookup is via the real manager attribute.
        pipeline._mediamtx = MagicMock()
        pipeline._mediamtx.rtsp_port = 8554

        # start_wfb_tee() first calls _kill_orphan_wfb_tee_ffmpegs(),
        # which spawns ``pgrep`` to find stale tee processes. Capture
        # only the ffmpeg/bash spawn (the real wfb_tee) for argv
        # assertions; pgrep gets an empty-stdout proc so the orphan
        # sweep finishes silently.
        captured_cmd: list[str] = []

        async def _fake_exec(*cmd, **_kw):
            proc = MagicMock()
            proc.returncode = None
            proc.pid = 9999
            proc.stderr = None
            if cmd and cmd[0] == "pgrep":
                proc.communicate = AsyncMock(return_value=(b"", b""))
                return proc
            captured_cmd.extend(cmd)
            return proc

        monkeypatch.setattr(pl_mod.asyncio, "create_subprocess_exec", _fake_exec)

        ok = await pipeline.start_wfb_tee()
        assert ok is True
        # The tee reads the local mediamtx RTSP path and forwards as
        # RTP to 127.0.0.1:5600 (the wfb_tx UDP socket). The earlier
        # raw-UDP form was replaced by RTP framing so the receiver can
        # demux + reorder; the wire transport underneath is still UDP.
        assert "rtsp://localhost:8554/main" in captured_cmd
        joined = " ".join(captured_cmd)
        assert "rtp://127.0.0.1:5600" in joined
        assert "pkt_size=1316" in joined
        # No re-encode: the encoder is upstream and CPU is precious.
        assert "-c:v" in captured_cmd
        copy_index = captured_cmd.index("-c:v") + 1
        assert captured_cmd[copy_index] == "copy"
        # RTP-specific framing flags must be in the argv.
        assert "-f" in captured_cmd
        assert "rtp" in captured_cmd
        assert "-payload_type" in captured_cmd

    @pytest.mark.asyncio
    async def test_start_wfb_tee_shape_unchanged_by_sei_latency_flag(
        self, monkeypatch,
    ):
        """The wfb_tee no longer changes shape based on
        ``wfb.sei_latency``: SEI injection now lives upstream of
        mediamtx (see ``wrap_with_sei_inject`` at start_stream), so the
        stream the tee pulls already carries one SEI NAL per VCL slice
        when the flag is set. The tee itself is always a plain
        RTSP -> RTP copy regardless. Previously this test expected a
        bash-wrapped three-stage pipeline; that wrapper was retired
        when the injection moved upstream.
        """
        from ados.services.video import pipeline as pl_mod

        cfg = VideoConfig()
        cfg.wfb.sei_latency = True
        pipeline = VideoPipeline(cfg)
        pipeline._state = PipelineState.RUNNING
        pipeline._mediamtx = MagicMock()
        pipeline._mediamtx.rtsp_port = 8554

        captured_cmd: list[str] = []

        async def _fake_exec(*cmd, **_kw):
            proc = MagicMock()
            proc.returncode = None
            proc.pid = 9999
            proc.stderr = None
            if cmd and cmd[0] == "pgrep":
                proc.communicate = AsyncMock(return_value=(b"", b""))
                return proc
            captured_cmd.extend(cmd)
            return proc

        monkeypatch.setattr(pl_mod.asyncio, "create_subprocess_exec", _fake_exec)

        ok = await pipeline.start_wfb_tee()
        assert ok is True
        # Plain ffmpeg — no bash wrapper even when sei_latency=True.
        assert captured_cmd[0] == "ffmpeg"
        assert "bash" not in captured_cmd
        assert "ados.services.video.sei_injector" not in " ".join(captured_cmd)
        # Same RTSP -> RTP copy as the sei_latency=False case.
        assert "rtsp://localhost:8554/main" in captured_cmd
        joined = " ".join(captured_cmd)
        assert "rtp://127.0.0.1:5600" in joined
        copy_index = captured_cmd.index("-c:v") + 1
        assert captured_cmd[copy_index] == "copy"

    @pytest.mark.asyncio
    async def test_start_wfb_tee_skipped_when_pipeline_not_running(self, monkeypatch):
        from ados.services.video import pipeline as pl_mod

        pipeline = VideoPipeline(VideoConfig())
        pipeline._state = PipelineState.STOPPED

        called = []

        async def _fake_exec(*cmd, **_kw):
            called.append(cmd)
            return MagicMock(returncode=None, pid=1, stderr=None)

        monkeypatch.setattr(pl_mod.asyncio, "create_subprocess_exec", _fake_exec)
        ok = await pipeline.start_wfb_tee()
        assert ok is False
        assert called == []

    @pytest.mark.asyncio
    async def test_start_wfb_tee_idempotent_when_already_running(self):
        pipeline = VideoPipeline(VideoConfig())
        pipeline._state = PipelineState.RUNNING
        running = MagicMock()
        running.returncode = None
        pipeline._wfb_tee_process = running
        ok = await pipeline.start_wfb_tee()
        assert ok is True
        # Did not respawn — the existing process is reused.
        assert pipeline._wfb_tee_process is running

    @pytest.mark.asyncio
    async def test_start_wfb_tee_handles_missing_ffmpeg(self, monkeypatch):
        from ados.services.video import pipeline as pl_mod

        pipeline = VideoPipeline(VideoConfig())
        pipeline._state = PipelineState.RUNNING
        pipeline._mediamtx = MagicMock()
        pipeline._mediamtx.rtsp_port = 8554

        async def _missing(*_args, **_kw):
            raise FileNotFoundError("ffmpeg")

        monkeypatch.setattr(pl_mod.asyncio, "create_subprocess_exec", _missing)
        ok = await pipeline.start_wfb_tee()
        # Best-effort: returns False but does not raise. Pipeline keeps
        # running because the local mediamtx and cloud push do not need
        # the tee.
        assert ok is False

    @pytest.mark.asyncio
    async def test_stop_wfb_tee_terminates_subprocess(self, monkeypatch):
        """stop_wfb_tee() prefers process-group SIGTERM via os.killpg()
        because the tee subprocess is a bash wrapper with ffmpeg
        children (start_new_session=True at spawn). It only falls back
        to proc.terminate() when os.getpgid() can't resolve the group.
        Verify the SIGTERM-to-group path fires when pgid is available.
        """
        from ados.services.video.pipeline import wfb_tee as wfb_tee_mod

        pipeline = VideoPipeline(VideoConfig())
        proc = MagicMock()
        proc.returncode = None
        proc.pid = 9999
        # wait() resolves normally inside the 5s wait_for budget.
        proc.wait = AsyncMock(return_value=0)
        proc.terminate = MagicMock()
        proc.kill = MagicMock()
        pipeline._wfb_tee_process = proc

        # Resolve a fake pgid so the killpg branch fires.
        monkeypatch.setattr(wfb_tee_mod.os, "getpgid", lambda pid: 12345)
        killpg_calls: list[tuple[int, int]] = []
        monkeypatch.setattr(
            wfb_tee_mod.os, "killpg",
            lambda pgid, sig: killpg_calls.append((pgid, sig)),
        )
        # Stub the orphan-sweep pgrep so it returns no orphans.
        async def _no_orphans(*_args, **_kw):
            stub = MagicMock()
            stub.communicate = AsyncMock(return_value=(b"", b""))
            return stub
        monkeypatch.setattr(
            wfb_tee_mod.asyncio, "create_subprocess_exec", _no_orphans,
        )

        await pipeline.stop_wfb_tee()

        # SIGTERM landed on the resolved process group, not on the
        # bare proc handle (which would orphan the bash children).
        import signal as _signal
        assert (12345, _signal.SIGTERM) in killpg_calls
        proc.terminate.assert_not_called()
        proc.wait.assert_awaited()
        assert pipeline._wfb_tee_process is None

    @pytest.mark.asyncio
    async def test_stop_wfb_tee_no_op_when_already_stopped(self):
        pipeline = VideoPipeline(VideoConfig())
        # No process attached: stop is a clean no-op.
        await pipeline.stop_wfb_tee()
        assert pipeline._wfb_tee_process is None

    @pytest.mark.asyncio
    async def test_check_wfb_tee_health_detects_exit(self):
        pipeline = VideoPipeline(VideoConfig())
        proc = MagicMock()
        proc.returncode = 1  # ffmpeg crashed
        pipeline._wfb_tee_process = proc

        ok = await pipeline._check_wfb_tee_health()
        assert ok is False
        # Failed health drops the handle so the run loop can respawn.
        assert pipeline._wfb_tee_process is None

    @pytest.mark.asyncio
    async def test_check_wfb_tee_health_returns_true_when_not_started(self):
        pipeline = VideoPipeline(VideoConfig())
        # No tee attached — that is a healthy state pre-stream-start.
        ok = await pipeline._check_wfb_tee_health()
        assert ok is True

    def test_get_status_surfaces_wfb_tee_flag(self):
        pipeline = VideoPipeline(VideoConfig())
        # Stopped pipeline: tee not running.
        status = pipeline.get_status()
        assert status["wfb_tee"] is False

        # Inject a live tee process and verify the flag flips.
        proc = MagicMock()
        proc.returncode = None
        pipeline._wfb_tee_process = proc
        status = pipeline.get_status()
        assert status["wfb_tee"] is True


class TestEncoderOrphanKill:
    """Sweep stray ffmpegs from a previous pipeline cycle before
    spawning a fresh encoder + bridge, so the new processes do not
    fight an orphan for the V4L2 device or the mediamtx publisher slot.

    The encoder is also spawned with start_new_session=True so future
    teardowns can clean up the whole process group via os.killpg().
    """

    @pytest.mark.asyncio
    async def test_kill_orphan_encoder_ffmpegs_matches_device_path(
        self, monkeypatch,
    ):
        from ados.services.video import pipeline as pl_mod

        pipeline = VideoPipeline(VideoConfig())

        captured_pgrep_cmd: list = []

        async def _fake_exec(*cmd, **_kw):
            captured_pgrep_cmd.append(cmd)
            proc = MagicMock()
            # Two PIDs in pgrep output. communicate() returns (stdout, stderr).
            proc.communicate = AsyncMock(return_value=(b"12345\n12346\n", b""))
            return proc

        monkeypatch.setattr(pl_mod.asyncio, "create_subprocess_exec", _fake_exec)

        killed: list[tuple[int, int]] = []

        def _fake_kill(pid, sig):
            killed.append((pid, sig))

        # The orphan-kill methods live in pipeline.pipeline (the submodule),
        # which has `import os` at the top. Patch the os module so any call
        # to os.kill from that module is captured.
        monkeypatch.setattr("os.kill", _fake_kill)

        logged: list[dict] = []

        def _fake_warning(event, **kw):
            logged.append({"event": event, **kw})

        log_stub = MagicMock()
        log_stub.warning = _fake_warning
        monkeypatch.setattr(pl_mod, "log", log_stub, raising=False)

        await pipeline._kill_orphan_encoder_ffmpegs("/dev/video0")

        # Two pgrep calls fire per orphan-kill: first for the ffmpeg
        # ``-i /dev/videoN`` pattern, second for the rpicam-vid Pi-CSI
        # path. Both use ``--`` so a leading ``-`` in the pattern is
        # treated literally instead of as a flag.
        assert len(captured_pgrep_cmd) == 2
        assert captured_pgrep_cmd[0][0] == "pgrep"
        assert "--" in captured_pgrep_cmd[0]
        assert "-i /dev/video0" in captured_pgrep_cmd[0]
        assert captured_pgrep_cmd[1][0] == "pgrep"
        assert "--" in captured_pgrep_cmd[1]
        assert "rpicam-vid" in captured_pgrep_cmd[1]

        # Both PIDs from each pgrep output were SIGKILL'd (4 total —
        # the fake stdout returns the same two PIDs on every call;
        # SIGKILL on a missing PID is silently swallowed).
        import signal as _signal

        assert (12345, _signal.SIGKILL) in killed
        assert (12346, _signal.SIGKILL) in killed
        # Never signal our own process.
        assert all(pid != os.getpid() for pid, _ in killed)

        # Structured log emitted with device + count covering both
        # patterns (4 total kills from two pgreps each returning two
        # PIDs).
        kills_logged = [e for e in logged if e["event"] == "encoder_orphans_killed"]
        assert len(kills_logged) == 1
        assert kills_logged[0]["device"] == "/dev/video0"
        # Two pgreps each returned (12345, 12346) → 4 kills total.
        assert kills_logged[0]["count"] == 4

    @pytest.mark.asyncio
    async def test_kill_orphan_bridge_ffmpegs_matches_rtsp_url(
        self, monkeypatch,
    ):
        from ados.services.video import pipeline as pl_mod

        pipeline = VideoPipeline(VideoConfig())

        captured_pgrep_cmd: list = []

        async def _fake_exec(*cmd, **_kw):
            captured_pgrep_cmd.append(cmd)
            proc = MagicMock()
            proc.communicate = AsyncMock(return_value=(b"22345\n22346\n", b""))
            return proc

        monkeypatch.setattr(pl_mod.asyncio, "create_subprocess_exec", _fake_exec)

        killed: list[tuple[int, int]] = []

        def _fake_kill(pid, sig):
            killed.append((pid, sig))

        monkeypatch.setattr("os.kill", _fake_kill)

        logged: list[dict] = []

        def _fake_warning(event, **kw):
            logged.append({"event": event, **kw})

        log_stub = MagicMock()
        log_stub.warning = _fake_warning
        monkeypatch.setattr(pl_mod, "log", log_stub, raising=False)

        await pipeline._kill_orphan_bridge_ffmpegs(
            "rtsp://localhost:8554/main",
        )

        # pgrep cmd was issued, used `--` to force literal pattern, and
        # matched the RTSP URL.
        assert len(captured_pgrep_cmd) == 1
        assert captured_pgrep_cmd[0][0] == "pgrep"
        assert "--" in captured_pgrep_cmd[0]
        assert "rtsp://localhost:8554/main" in captured_pgrep_cmd[0]

        import signal as _signal

        assert (22345, _signal.SIGKILL) in killed
        assert (22346, _signal.SIGKILL) in killed
        assert all(pid != os.getpid() for pid, _ in killed)

        kills_logged = [e for e in logged if e["event"] == "bridge_orphans_killed"]
        assert len(kills_logged) == 1
        assert kills_logged[0]["destination"] == "rtsp://localhost:8554/main"
        assert kills_logged[0]["count"] == 2

    @pytest.mark.asyncio
    async def test_start_stream_kills_orphans_before_encoder_spawn(
        self, monkeypatch,
    ):
        from ados.services.video.encoder import EncoderType

        pipeline = _pipeline_with_mediamtx_mocks()
        monkeypatch.setattr(
            "ados.services.video.pipeline.detect_encoder_for_camera",
            lambda cam: EncoderType.FFMPEG,
        )
        monkeypatch.setattr(
            "ados.services.video.pipeline.build_encoder_command",
            lambda *a, **k: ["ffmpeg", "-i", "/dev/video0", "rtsp://x"],
        )

        # Track call order across the orphan-kill methods and the
        # encoder subprocess spawn.
        call_log: list[str] = []

        async def _fake_encoder_kill(device_path):
            call_log.append(f"kill_encoder:{device_path}")

        async def _fake_bridge_kill(rtsp_url):
            call_log.append(f"kill_bridge:{rtsp_url}")

        pipeline._kill_orphan_encoder_ffmpegs = AsyncMock(
            side_effect=_fake_encoder_kill
        )
        pipeline._kill_orphan_bridge_ffmpegs = AsyncMock(
            side_effect=_fake_bridge_kill
        )
        # Don't actually fire the radio fan-out during this test.
        pipeline.start_wfb_tee = AsyncMock(return_value=True)

        async def _fake_spawn(*cmd, **_kw):
            call_log.append("encoder_spawn")
            proc = MagicMock()
            proc.returncode = None
            proc.pid = 9999
            proc.stderr = None
            return proc

        import asyncio as _asyncio
        monkeypatch.setattr(_asyncio, "create_subprocess_exec", _fake_spawn)

        ok = await pipeline.start_stream()
        assert ok is True

        # Both orphan sweeps must happen before the encoder spawns.
        encoder_kill_idx = next(
            i for i, e in enumerate(call_log) if e.startswith("kill_encoder:")
        )
        bridge_kill_idx = next(
            i for i, e in enumerate(call_log) if e.startswith("kill_bridge:")
        )
        spawn_idx = call_log.index("encoder_spawn")
        assert encoder_kill_idx < spawn_idx
        assert bridge_kill_idx < spawn_idx
        # Device path + RTSP URL propagated through.
        assert "kill_encoder:/dev/video0" in call_log
        assert "kill_bridge:rtsp://localhost:8554/main" in call_log

    @pytest.mark.asyncio
    async def test_encoder_spawn_uses_start_new_session(self, monkeypatch):
        from ados.services.video.encoder import EncoderType

        pipeline = _pipeline_with_mediamtx_mocks()
        monkeypatch.setattr(
            "ados.services.video.pipeline.detect_encoder_for_camera",
            lambda cam: EncoderType.FFMPEG,
        )
        monkeypatch.setattr(
            "ados.services.video.pipeline.build_encoder_command",
            lambda *a, **k: ["ffmpeg", "-i", "/dev/video0", "rtsp://x"],
        )
        # Bypass the orphan sweeps so the spawn we inspect is the
        # encoder, not the pgrep child process.
        pipeline._kill_orphan_encoder_ffmpegs = AsyncMock()
        pipeline._kill_orphan_bridge_ffmpegs = AsyncMock()
        pipeline.start_wfb_tee = AsyncMock(return_value=True)

        captured_kwargs: list[dict] = []

        async def _fake_spawn(*cmd, **kwargs):
            captured_kwargs.append(kwargs)
            proc = MagicMock()
            proc.returncode = None
            proc.pid = 9999
            proc.stderr = None
            return proc

        import asyncio as _asyncio
        monkeypatch.setattr(_asyncio, "create_subprocess_exec", _fake_spawn)

        ok = await pipeline.start_stream()
        assert ok is True

        # Exactly one spawn (the encoder) was issued.
        assert len(captured_kwargs) == 1
        # The encoder must be put in a new session so the supervisor
        # can clean up the whole process group with os.killpg() on a
        # graceful teardown.
        assert captured_kwargs[0].get("start_new_session") is True

    @pytest.mark.asyncio
    async def test_kill_orphan_encoder_rpicam_matches_pattern(
        self, monkeypatch,
    ):
        """The encoder sweep also covers rpicam-vid orphans for the Pi
        CSI camera path. rpicam doesn't open ``/dev/video*`` directly
        so the ``-i /dev/videoN`` pattern misses it; the second pgrep
        sweep with the bare ``rpicam-vid`` pattern catches it.
        """
        from ados.services.video import pipeline as pl_mod

        pipeline = VideoPipeline(VideoConfig())

        captured_patterns: list = []

        # Two pgrep calls happen inside one orphan-kill: first for
        # ``-i {device_path}`` (returns empty), second for
        # ``rpicam-vid`` (returns two PIDs). Use a side-effect list to
        # serve both.
        call_count = {"n": 0}

        async def _fake_exec(*cmd, **_kw):
            captured_patterns.append(cmd[-1])
            call_count["n"] += 1
            proc = MagicMock()
            if call_count["n"] == 1:
                # First call: ffmpeg encoder pattern, no matches.
                proc.communicate = AsyncMock(return_value=(b"", b""))
            else:
                # Second call: rpicam pattern, two orphan PIDs.
                proc.communicate = AsyncMock(
                    return_value=(b"42000\n42001\n", b""),
                )
            return proc

        monkeypatch.setattr(pl_mod.asyncio, "create_subprocess_exec", _fake_exec)

        killed: list[tuple[int, int]] = []

        def _fake_kill(pid, sig):
            killed.append((pid, sig))

        monkeypatch.setattr("os.kill", _fake_kill)

        logged: list[dict] = []

        def _fake_warning(event, **kw):
            logged.append({"event": event, **kw})

        log_stub = MagicMock()
        log_stub.warning = _fake_warning
        monkeypatch.setattr(pl_mod, "log", log_stub, raising=False)

        await pipeline._kill_orphan_encoder_ffmpegs("/dev/video0")

        # Two pgrep calls fired: one per pattern, in this order.
        assert call_count["n"] == 2
        assert captured_patterns[0] == "-i /dev/video0"
        assert captured_patterns[1] == "rpicam-vid"

        # Two rpicam orphans got SIGKILL'd.
        import signal as _signal
        assert (42000, _signal.SIGKILL) in killed
        assert (42001, _signal.SIGKILL) in killed

        # Combined count surfaced in the structured log.
        kills_logged = [e for e in logged if e["event"] == "encoder_orphans_killed"]
        assert len(kills_logged) == 1
        assert kills_logged[0]["count"] == 2

    @pytest.mark.asyncio
    async def test_start_cloud_push_carries_start_new_session_true(
        self, monkeypatch,
    ):
        """``cloud_push`` ffmpeg must spawn with
        ``start_new_session=True`` so its lifecycle isn't tied to the
        parent agent process group. Without it, an unclean parent death
        leaves the cloud_push ffmpeg holding a TCP connection to the
        remote RTSP server forever.
        """
        from ados.services.video import pipeline as pl_mod

        pipeline = _pipeline_with_mediamtx_mocks()
        pipeline._state = pl_mod.PipelineState.RUNNING
        # Configure a cloud relay URL so start_cloud_push() doesn't
        # short-circuit on the "not configured" path.
        pipeline._config = type(
            "Cfg",
            (),
            {
                "cloud_relay_url": "rtsp://relay.example.com:8554",
                "wfb": type("W", (), {"sei_latency": False})(),
            },
        )()

        captured_kwargs: list[dict] = []

        async def _fake_spawn(*cmd, **kwargs):
            captured_kwargs.append(kwargs)
            proc = MagicMock()
            proc.returncode = None
            proc.pid = 9999
            proc.stderr = None
            return proc

        # _drain_stderr starts a background task; stub it so the test
        # doesn't leak coroutines.
        pipeline._drain_stderr = AsyncMock()

        import asyncio as _asyncio
        monkeypatch.setattr(_asyncio, "create_subprocess_exec", _fake_spawn)

        await pipeline.start_cloud_push()

        # Exactly one cloud_push spawn was issued.
        assert len(captured_kwargs) == 1
        # session isolation is on so process-group cleanup is possible.
        assert captured_kwargs[0].get("start_new_session") is True
