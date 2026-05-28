"""Shared asyncio utilities."""
from __future__ import annotations

import asyncio

import structlog

log = structlog.get_logger(__name__)


def log_task_exceptions(task: asyncio.Task) -> None:
    """Done-callback that logs exceptions from background tasks.

    Pass to ``task.add_done_callback`` so a crashed background task does not
    silently disappear.
    """
    if task.cancelled():
        return
    exc = task.exception()
    if exc is not None:
        log.error(
            "background_task_failed",
            task=task.get_name(),
            error=str(exc),
            exc_info=exc,
        )
