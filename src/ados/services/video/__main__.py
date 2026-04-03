"""Standalone video pipeline service.

Manages camera detection, hardware encoding, mediamtx streaming, and
optional cloud RTSP push. Monitors encoder health and auto-restarts on failure.

Run: python -m ados.services.video
"""

from __future__ import annotations

import asyncio
import signal
import sys

import structlog

from ados.core.config import load_config
from ados.core.logging import configure_logging


async def main() -> None:
    config = load_config()
    configure_logging(config.logging.level)
    log = structlog.get_logger()
    log.info("video_service_starting")

    if config.video.mode == "disabled":
        log.info("video_service_disabled", msg="video.mode is 'disabled' in config, exiting")
        return

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    from ados.services.video.pipeline import VideoPipeline

    pipeline = VideoPipeline(config.video)

    # Start the stream
    started = await pipeline.start_stream()
    if started:
        log.info("video_stream_started")
        # Optionally push to cloud relay
        if config.video.cloud_relay_url:
            await pipeline.start_cloud_push()
    else:
        log.warning("video_stream_failed_to_start", msg="pipeline will retry in health loop")

    # Run the pipeline health monitor loop
    pipeline_task = asyncio.create_task(pipeline.run(), name="video-pipeline")

    log.info("video_service_ready")

    # Wait for shutdown
    await shutdown.wait()

    log.info("video_service_stopping")
    pipeline_task.cancel()
    await asyncio.gather(pipeline_task, return_exceptions=True)
    await pipeline.stop_stream()
    log.info("video_service_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
