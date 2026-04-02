"""Resilient task wrapper — exponential backoff for non-critical services.

Ported from ADOS Agent Lite. Wraps an async callable so that crashes
in one service don't bring down the entire agent. Failed services are
restarted with exponential backoff up to a configurable maximum.
"""

from __future__ import annotations

import asyncio
import time
from typing import Awaitable, Callable

from ados.core.logging import get_logger

log = get_logger("core.resilient")


async def resilient_task(
    coro_func: Callable[[], Awaitable[None]],
    name: str,
    shutdown_event: asyncio.Event,
    max_backoff: float = 60.0,
    service_start_times: dict[str, float] | None = None,
) -> None:
    """Run a zero-arg async callable with exponential backoff on failure.

    Non-critical services use this wrapper so a crash in one service
    doesn't bring down the whole agent. ``coro_func`` must be an async
    callable that takes NO arguments (bind state via closure or class).

    Parameters
    ----------
    coro_func : Callable[[], Awaitable[None]]
        The async function to run.
    name : str
        Human-readable service name for logging.
    shutdown_event : asyncio.Event
        Set this to stop the service and its restart loop.
    max_backoff : float
        Maximum seconds between restart attempts.
    service_start_times : dict or None
        Optional dict to record service start timestamps (monotonic).
    """
    backoff = 1.0
    while not shutdown_event.is_set():
        try:
            if service_start_times is not None:
                service_start_times[name] = time.monotonic()
            log.info("service_start", service=name)
            await coro_func()
            return
        except asyncio.CancelledError:
            log.info("service_cancelled", service=name)
            return
        except Exception:
            log.exception("service_crashed", service=name, restart_in=backoff)
            try:
                await asyncio.wait_for(shutdown_event.wait(), timeout=backoff)
                return
            except asyncio.TimeoutError:
                pass
            backoff = min(backoff * 2, max_backoff)
