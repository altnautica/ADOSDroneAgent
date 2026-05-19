"""Video tap mixin for the OLED service.

The video tap reads the local mediamtx stream and renders it to the
LCD video page when active. Lifecycle is managed by a keep-alive task
plus a stats-persistence task plus an idle-teardown trigger from the
render loop. All state lives on the parent ``OledService`` instance;
this mixin only contributes methods.
"""

from __future__ import annotations

import asyncio
from typing import Any


class _VideoTapMixin:
    """Methods that drive the LocalVideoTap lifecycle on the OLED service.

    Composed into ``OledService`` via the MRO in ``lifecycle.py``. No
    ``__init__`` here — all state (``self._page_context``, ``self._stop``,
    etc.) is declared in ``OledService.__init__`` so the mixin can read
    and write it freely via ``self``.
    """

    async def _maybe_tear_down_video_tap(self) -> None:
        """Tear down the video page's local tap after the inactivity grace.

        The video page can't observe its own absence; the render loop
        drives the timer based on whether the active page is the
        ``video`` page. We call ``maybe_teardown_idle_tap`` on the
        registered VideoPage instance whenever the active route is
        elsewhere — the helper short-circuits when the inactivity
        threshold hasn't elapsed yet, so the cost is one method call
        per tick when the operator is on a different tab.
        """
        from .service import log

        if (
            self._page_navigator is None
            or self._page_context is None
            or self._page_navigator.active_page_id == "video"
        ):
            return
        video_page = self._page_navigator.page("video")
        if video_page is None:
            return
        helper = getattr(video_page, "maybe_teardown_idle_tap", None)
        if helper is None:
            return
        try:
            await helper(self._page_context)
        except Exception as exc:  # noqa: BLE001
            log.debug("video_tap_idle_teardown_failed", error=str(exc))

    def _build_video_tap(self) -> Any:
        """Construct the always-on LocalVideoTap, picking up the
        operator's `video.lcd_fps_cap` config so a faster SBC can ask
        for higher fps without code changes. Returns None if the tap
        module fails to import (gstreamer missing in test envs).
        """
        from .service import log

        try:
            from ados.core.config import load_config
            from ados.services.video.local_tap import LocalVideoTap
        except Exception as exc:  # noqa: BLE001
            log.warning("video_tap_module_unavailable", error=str(exc))
            return None
        fps_cap = 15
        try:
            cfg = load_config()
            fps_cap = int(getattr(cfg.video, "lcd_fps_cap", 15) or 15)
        except Exception as exc:  # noqa: BLE001
            log.debug("video_tap_config_load_failed", error=str(exc))
        return LocalVideoTap(fps_cap=fps_cap, logger=log)

    async def _persist_tap_stats_forever(self) -> None:
        """Persist LocalVideoTap stats to /run/ados/lcd-latency.json.

        The API service is a separate process and cannot read the
        tap's in-memory state directly. Drop a JSON snapshot every
        second so /api/video/latency has something to serve. Best
        effort: I/O failures are logged at debug and the loop
        continues.
        """
        from .service import log

        if self._page_context is None:
            return
        tap = self._page_context.video_tap
        if tap is None:
            return
        while not self._stop.is_set():
            try:
                tap.persist_stats_to_file()
            except Exception as exc:  # noqa: BLE001
                log.debug("oled_tap_stats_persist_failed", error=str(exc))
            # Also publish the tap-status snapshot the heartbeat enricher
            # reads so the cloud surface has video state even when the
            # operator hasn't navigated to the video LCD page yet. The
            # video page's own metrics tick still overwrites this file
            # with page-level state (recording flag) when it's active.
            try:
                tap.persist_tap_status_to_file()
            except Exception as exc:  # noqa: BLE001
                log.debug("oled_tap_status_persist_failed", error=str(exc))
            try:
                await asyncio.wait_for(self._stop.wait(), timeout=1.0)
            except TimeoutError:
                pass

    async def _ensure_video_tap_forever(self) -> None:
        """Keep the LCD video tap running for the agent's lifetime.

        Calls ``tap.start()`` with the existing internal 15s cooldown
        until mediamtx-gs is publishing /main. Once started, the tap's
        own bus-error restart loop handles transient errors. This
        coroutine sleeps once the tap reaches a stable state so the
        agent doesn't busy-loop on a healthy rig.
        """
        from ados.services.video.local_tap import LocalVideoTapUnavailable

        from .service import log

        if self._page_context is None:
            return
        tap = self._page_context.video_tap
        if tap is None:
            return
        backoff_s = 5.0
        while not self._stop.is_set():
            try:
                await tap.start()
                # tap.start() returns once the pipeline is wired and
                # the run-loop thread is spawned. Subsequent recovery
                # is internal. Sleep until shutdown.
                log.info("oled_video_tap_started")
                try:
                    await self._stop.wait()
                except Exception:  # noqa: BLE001
                    pass
                return
            except LocalVideoTapUnavailable as exc:
                log.warning(
                    "oled_video_tap_start_failed",
                    error=str(exc),
                    retry_in_s=backoff_s,
                )
            except Exception as exc:  # noqa: BLE001
                log.warning(
                    "oled_video_tap_start_error",
                    error=str(exc),
                    retry_in_s=backoff_s,
                )
            try:
                await asyncio.wait_for(self._stop.wait(), timeout=backoff_s)
                return
            except TimeoutError:
                continue
