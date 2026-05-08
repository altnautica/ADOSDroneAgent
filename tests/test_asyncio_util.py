"""Tests for ``ados.core.asyncio_util.log_task_exceptions``.

Cover normal completion (no log), exception (logged), and cancelled
(no log). The structlog logger is intercepted via monkeypatching so
the assertion is on the side-effect, not on log output capture.
"""

from __future__ import annotations

import asyncio
from typing import Any

import pytest

from ados.core import asyncio_util


class _RecordingLogger:
    def __init__(self) -> None:
        self.errors: list[tuple[str, dict[str, Any]]] = []

    def error(self, event: str, **kwargs: Any) -> None:
        self.errors.append((event, kwargs))


@pytest.fixture
def recorder(monkeypatch: pytest.MonkeyPatch) -> _RecordingLogger:
    rec = _RecordingLogger()
    monkeypatch.setattr(asyncio_util, "log", rec)
    return rec


async def test_log_task_exceptions_normal_completion_no_log(
    recorder: _RecordingLogger,
) -> None:
    async def _ok() -> int:
        return 7

    task = asyncio.create_task(_ok())
    task.add_done_callback(asyncio_util.log_task_exceptions)
    result = await task
    assert result == 7
    # Allow the done-callback to fire.
    await asyncio.sleep(0)
    assert recorder.errors == []


async def test_log_task_exceptions_logs_on_failure(
    recorder: _RecordingLogger,
) -> None:
    async def _boom() -> None:
        raise RuntimeError("boom")

    task = asyncio.create_task(_boom(), name="boom-task")
    task.add_done_callback(asyncio_util.log_task_exceptions)
    with pytest.raises(RuntimeError):
        await task
    await asyncio.sleep(0)
    assert len(recorder.errors) == 1
    event, payload = recorder.errors[0]
    assert event == "background_task_failed"
    assert payload["task"] == "boom-task"
    assert "boom" in payload["error"]


async def test_log_task_exceptions_cancelled_no_log(
    recorder: _RecordingLogger,
) -> None:
    async def _sleeper() -> None:
        await asyncio.sleep(60)

    task = asyncio.create_task(_sleeper())
    task.add_done_callback(asyncio_util.log_task_exceptions)
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task
    await asyncio.sleep(0)
    assert recorder.errors == []
