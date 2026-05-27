"""WFB radio fan-out sidecar for :class:`VideoPipeline`.

Owns the ffmpeg subprocess that re-encodes the local mediamtx RTSP
stream into RTP datagrams on UDP 5600 for the wfb-ng radio TX
process. The encoder + mediamtx + cloud-push lifecycles all stay
in ``pipeline.py``; only the wfb-tee subprocess management and its
output-progress watchdog (per Rule 37 — process-liveness alone is
never proof of work) live here.

What this mixin covers:

* :meth:`start_wfb_tee` — spawn the ffmpeg sidecar (with
  ``-progress pipe:2`` and ``start_new_session=True`` so the
  output-progress watchdog has a token to count and the group-kill
  path is uniform).
* :meth:`stop_wfb_tee` — SIGTERM/SIGKILL the whole process group so
  bash wrappers + their ffmpeg children die together.
* :meth:`_kill_orphan_wfb_tee_ffmpegs` — sweep stale ffmpegs from a
  previous, unclean run that would otherwise fight a freshly-spawned
  ffmpeg for UDP 5600 and corrupt the RTP stream.
* :meth:`_check_wfb_tee_health` — verify the ffmpeg is alive AND
  emitting progress; trigger restart on either failure.
* :meth:`_drain_wfb_tee_stderr` — drain stderr while stamping
  :attr:`_wfb_tee_last_progress_at` whenever a progress token is
  observed.

The mixin holds methods only — every attribute the methods touch
(``_wfb_tee_process``, ``_wfb_tee_stderr_task``,
``_wfb_tee_last_progress_at``, ``_wfb_tee_last_frame_count``,
``_wfb_tee_restart_count``) is declared in
:class:`VideoPipeline.__init__` over in ``pipeline.py``.
"""

from __future__ import annotations

import asyncio
import os
import signal
import time

from .constants import (
    _FFMPEG_FRAME_PROGRESS_RE,
    _FFMPEG_PROGRESS_LINE_RE,
    _FFMPEG_PROGRESS_TOKEN_RE,
    _WFB_TEE_HOST,
    _WFB_TEE_PAYLOAD_TYPE,
    _WFB_TEE_PKT_SIZE,
    _WFB_TEE_PORT,
    _WFB_TEE_PROGRESS_TIMEOUT_S,
    _WFB_TEE_SSRC,
)


class _WfbTeeMixin:
    """WFB tee subprocess + watchdog grafted onto :class:`VideoPipeline`."""

    async def _drain_wfb_tee_stderr(
        self, proc: asyncio.subprocess.Process,
    ) -> None:
        """Drain wfb_tee stderr AND track ffmpeg's frame= progress.

        Every line that matches `_FFMPEG_FRAME_PROGRESS_RE` updates
        ``self._wfb_tee_last_progress_at`` so the health-check can
        tell the difference between a healthy ffmpeg (frames
        advancing) and a zombie (process alive, frames stuck). This
        is the output-byte-counter watchdog mandated by Rule 37 —
        process-liveness alone is never proof of work.
        """
        from .pipeline import _pkg

        log = _pkg().log
        if proc.stderr is None:
            return
        # Rate-limit the real-diagnostic logging path so an ffmpeg error
        # storm (e.g. a dead RTSP source retried hard) cannot flood the
        # journal or pin a CPU core. The per-second -progress telemetry
        # block is suppressed entirely below — it is parsed for the
        # liveness stamp but is noise in the log at ~12 lines/s.
        window_s = 10.0
        max_lines_per_window = 5
        window_start = time.monotonic()
        logged = 0
        suppressed = 0
        last_suppressed_line = ""
        try:
            while True:
                line = await proc.stderr.readline()
                if not line:
                    break
                text = line.decode(errors="replace").rstrip()
                if not text:
                    continue
                # Stamp progress on any of ffmpeg's status counters:
                # frame= (transcoding), size= / time= / bitrate=
                # (any output mode including -c copy). All advance at
                # roughly 1 Hz on a healthy bench. Stamping on the
                # mere presence of the token (not a strict increase)
                # is sufficient because ffmpeg's status line is
                # carriage-returned and re-emitted every second only
                # when it's actually processing.
                if _FFMPEG_PROGRESS_TOKEN_RE.search(text):
                    self._wfb_tee_last_progress_at = time.monotonic()
                    # Also try to pull a frame number for observability;
                    # may not advance with -c copy but harmless.
                    m = _FFMPEG_FRAME_PROGRESS_RE.search(text)
                    if m is not None:
                        try:
                            frame = int(m.group(1))
                            if frame > self._wfb_tee_last_frame_count:
                                self._wfb_tee_last_frame_count = frame
                        except (TypeError, ValueError):
                            pass
                # Suppress the routine -progress telemetry block from the
                # journal (parsed above for the liveness stamp). Only real
                # ffmpeg diagnostics reach the log.
                if _FFMPEG_PROGRESS_TOKEN_RE.search(text) or _FFMPEG_PROGRESS_LINE_RE.match(text):
                    continue
                now = time.monotonic()
                if now - window_start >= window_s:
                    if suppressed:
                        log.warning(
                            "subprocess_stderr_suppressed",
                            label="wfb_tee",
                            suppressed=suppressed,
                            window_s=round(now - window_start, 1),
                            last_line=last_suppressed_line,
                        )
                    window_start = now
                    logged = 0
                    suppressed = 0
                    last_suppressed_line = ""
                if logged < max_lines_per_window:
                    log.warning("subprocess_stderr", label="wfb_tee", line=text)
                    logged += 1
                else:
                    suppressed += 1
                    last_suppressed_line = text
        except (asyncio.CancelledError, Exception):
            pass

    async def start_wfb_tee(self) -> bool:
        """Fan out the encoded H.264 RTSP back to local UDP for the radio.

        The wfb-ng TX subprocess listens on UDP 127.0.0.1:5600 for raw
        Annex-B H.264 frames to encapsulate and broadcast over the
        radio. Without something feeding that socket, the radio link is
        a dry pipe even when the encoder is publishing fine to mediamtx.
        This sidecar reads from the local mediamtx RTSP path with
        `-c:v copy` (no re-encode) and writes to UDP. On rigs without a
        wfb radio (e.g. a ground station running this service for some
        reason) the UDP packets are silently dropped by the kernel —
        harmless cost.
        """
        from .pipeline import PipelineState, _pkg

        log = _pkg().log
        if self._state != PipelineState.RUNNING:
            log.warning("wfb_tee_skipped", reason="pipeline not running")
            return False

        # Idempotent: an existing healthy tee just stays.
        if (
            self._wfb_tee_process is not None
            and self._wfb_tee_process.returncode is None
        ):
            return True

        # Sweep orphans from a previous run before spawning a fresh
        # tee. Handles: older agent versions that didn't set
        # start_new_session, an unclean SIGKILL of the parent that
        # leaves bash dead but ffmpegs alive, or two-set duplication
        # from a failed restart cycle. Without this sweep, the new
        # ffmpeg fights an old one for the same RTP destination on
        # UDP 5600 and the LCD video freezes.
        await self._kill_orphan_wfb_tee_ffmpegs()

        local_rtsp = f"rtsp://localhost:{self._mediamtx.rtsp_port}/main"
        rtp_url = (
            f"rtp://{_WFB_TEE_HOST}:{_WFB_TEE_PORT}?pkt_size={_WFB_TEE_PKT_SIZE}"
        )

        # SEI latency injection now lives upstream of mediamtx (see the
        # wrap_with_sei_inject call in start_stream). The stream we
        # pull out of local_rtsp here already carries one SEI NAL per
        # VCL slice when wfb.sei_latency is set, so wfb_tee just does
        # a plain RTSP -> RTP copy with no extra processing. Tracking
        # sei_latency_on only for logging.
        sei_latency_on = bool(
            getattr(self._config.wfb, "sei_latency", False)
        )

        try:
            # `-progress pipe:2` forces ffmpeg to write its periodic
            # status report (frame=, size=, time=, bitrate=, ...) to
            # stderr as plain key=value lines, ONE PER SECOND. Without
            # this flag ffmpeg suppresses the status line entirely
            # when stderr is captured (not a tty); our watchdog can't
            # tell working-but-quiet from wedged-and-stuck, and
            # fires false-positive restarts every ~15 s.
            # `-muxdelay 0 -muxpreload 0 -flush_packets 1` strip the
            # RTP muxer's default 0.7 s mux delay + 0.5 s preload +
            # output-side packet aggregation. NB: do NOT add
            # `-max_delay 0` here — it breaks codec discovery on the
            # input ffmpeg (same root cause as the mediamtx-gs
            # ingest sidecar earlier).
            self._wfb_tee_process = await asyncio.create_subprocess_exec(
                "ffmpeg",
                "-fflags", "nobuffer",
                "-flags", "low_delay",
                "-rtsp_transport", "tcp",
                "-i", local_rtsp,
                "-c:v", "copy",
                "-f", "rtp",
                "-payload_type", str(_WFB_TEE_PAYLOAD_TYPE),
                "-ssrc", _WFB_TEE_SSRC,
                "-muxdelay", "0",
                "-muxpreload", "0",
                "-flush_packets", "1",
                "-progress", "pipe:2",
                rtp_url,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
                # start_new_session keeps the group-kill path uniform
                # with stop_wfb_tee's killpg-or-terminate dance.
                start_new_session=True,
            )
            # Reset the progress watchdog state at spawn time so a fresh
            # tee gets the full _WFB_TEE_PROGRESS_TIMEOUT_S window
            # before the health check trips on it.
            self._wfb_tee_last_progress_at = time.monotonic()
            self._wfb_tee_last_frame_count = -1
            self._wfb_tee_stderr_task = asyncio.create_task(
                self._drain_wfb_tee_stderr(self._wfb_tee_process)
            )
            log.info(
                "wfb_tee_started",
                source=local_rtsp,
                destination=rtp_url,
                payload_type=_WFB_TEE_PAYLOAD_TYPE,
                pid=self._wfb_tee_process.pid,
                sei_latency=sei_latency_on,
            )
            return True
        except FileNotFoundError:
            log.error("wfb_tee_ffmpeg_not_found")
            return False
        except Exception as exc:  # noqa: BLE001
            log.error("wfb_tee_start_failed", error=str(exc))
            return False

    async def stop_wfb_tee(self) -> None:
        """Stop the wfb radio UDP tee.

        Sends SIGTERM/SIGKILL to the entire process group (start_new_
        session=True at spawn time) so the bash wrapper AND its ffmpeg
        children all die together. Without this, killing bash alone
        orphans the ffmpegs; the next start cycle spawns NEW ffmpegs
        alongside the orphans, two compete for the same RTSP source +
        UDP 5600 destination, RTP packets garble, and the LCD freezes.
        """
        from .pipeline import _pkg

        log = _pkg().log
        if self._wfb_tee_stderr_task is not None:
            self._wfb_tee_stderr_task.cancel()
            self._wfb_tee_stderr_task = None
        proc = self._wfb_tee_process
        if proc is not None and proc.returncode is None:
            pgid: int | None = None
            try:
                pgid = os.getpgid(proc.pid)
            except (ProcessLookupError, OSError):
                pgid = None
            # Send SIGTERM to the whole group when we can; fall back to
            # the single-process terminate() for safety.
            try:
                if pgid is not None:
                    os.killpg(pgid, signal.SIGTERM)
                else:
                    proc.terminate()
            except (ProcessLookupError, OSError):
                pass
            try:
                await asyncio.wait_for(proc.wait(), timeout=5.0)
            except (TimeoutError, asyncio.CancelledError):
                try:
                    if pgid is not None:
                        os.killpg(pgid, signal.SIGKILL)
                    else:
                        proc.kill()
                except (ProcessLookupError, OSError):
                    pass
            log.info("wfb_tee_stopped", pgid=pgid)
        # Belt-and-suspenders: even after our managed process is gone,
        # there could be orphan ffmpegs left from an older code
        # version or from an unclean exit. Sweep them.
        await self._kill_orphan_wfb_tee_ffmpegs()
        self._wfb_tee_process = None

    async def _kill_orphan_wfb_tee_ffmpegs(self) -> None:
        """Kill any stray ffmpegs that match the wfb_tee command signature.

        Defence-in-depth: an orphan ffmpeg sending to UDP 5600 will
        fight a freshly-spawned one and corrupt the RTP stream. We
        identify orphans by their command line (sending to the wfb
        RTP destination) and SIGKILL them.
        """
        from .pipeline import _pkg

        log = _pkg().log
        try:
            proc = await asyncio.create_subprocess_exec(
                "pgrep", "-f", f"rtp://{_WFB_TEE_HOST}:{_WFB_TEE_PORT}",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=2.0)
        except (FileNotFoundError, TimeoutError, asyncio.CancelledError):
            return
        for line in stdout.decode("utf-8", errors="replace").splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                pid = int(line)
            except ValueError:
                continue
            # Skip our own python process if it's matched by name.
            if pid == os.getpid():
                continue
            try:
                os.kill(pid, signal.SIGKILL)
                log.warning(
                    "wfb_tee_orphan_killed",
                    pid=pid,
                    note="stale ffmpeg from a previous wfb_tee cycle",
                )
            except (ProcessLookupError, OSError):
                pass

    async def _check_wfb_tee_health(self) -> bool:
        """Check if the wfb UDP tee subprocess is still running AND producing.

        Returns True if healthy or if the tee was never started.
        Returns False when:
        1. The process exited (returncode set).
        2. The process is alive but ffmpeg's frame= counter hasn't
           advanced for _WFB_TEE_PROGRESS_TIMEOUT_S seconds (the
           process is a zombie — alive, holding ports, but not
           pushing UDP packets to wfb_tx). This is the "PLAYING but
           silent" failure mode that process-liveness checks miss.
           Per Rule 37, process-liveness is never proof of work.
        """
        from .pipeline import _pkg

        log = _pkg().log
        if self._wfb_tee_process is None:
            return True
        if self._wfb_tee_process.returncode is not None:
            log.warning(
                "wfb_tee_process_exited",
                returncode=self._wfb_tee_process.returncode,
            )
            self._wfb_tee_process = None
            if self._wfb_tee_stderr_task is not None:
                self._wfb_tee_stderr_task.cancel()
                self._wfb_tee_stderr_task = None
            return False
        # Output-progress watchdog: ffmpeg should be advancing its
        # frame counter at ~30 Hz. If we haven't seen a fresh `frame=N`
        # for the timeout window, the encoder pipeline is silently
        # stuck. _wfb_tee_last_progress_at is stamped to monotonic()
        # both on spawn and on each frame advance, so a freshly-spawned
        # tee gets the full window before the check trips.
        silent_for = time.monotonic() - self._wfb_tee_last_progress_at
        if silent_for >= _WFB_TEE_PROGRESS_TIMEOUT_S:
            log.warning(
                "wfb_tee_zombie_detected",
                silent_s=round(silent_for, 1),
                threshold_s=_WFB_TEE_PROGRESS_TIMEOUT_S,
                last_frame=self._wfb_tee_last_frame_count,
                note="alive but ffmpeg frame counter flat; forcing restart",
            )
            return False
        return True
