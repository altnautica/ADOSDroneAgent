"""Signal handler registration + asyncio loop bootstrap.

The agent runs as a single supervised process. SIGTERM (from systemd
``Stop=``) and SIGINT (from a console operator) both trigger a graceful
shutdown via ``app.request_shutdown()``. We set the handlers via the
loop so the running coroutine wakes immediately rather than blocking
on a default signal-handler queue.
"""

from __future__ import annotations

import asyncio
import signal
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .app import AgentApp


def install_termination_handlers(
    loop: asyncio.AbstractEventLoop,
    app: AgentApp,
) -> None:
    """Wire SIGTERM and SIGINT to ``app.request_shutdown()``.

    Best-effort: on platforms where ``add_signal_handler`` is not
    available (Windows during tests), the registration silently
    falls back to the default behavior.
    """
    for sig in (signal.SIGTERM, signal.SIGINT):
        try:
            loop.add_signal_handler(sig, app.request_shutdown)
        except (NotImplementedError, RuntimeError):
            # NotImplementedError on Windows; RuntimeError when called
            # from a non-main thread or a closed loop. Either way the
            # supervisor falls back to KeyboardInterrupt handling in
            # the run-loop wrapper.
            pass


__all__ = ["install_termination_handlers"]
