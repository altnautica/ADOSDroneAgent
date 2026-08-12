"""AgentApp — top-level orchestrator that owns every long-running service.

The class itself is intentionally thin. Where a piece of logic could
live as a free function taking the app, it does: service spawn order
lives in ``service_registry.register_services``.

The instance methods that remain here are the ones that own private
mutable state (task list, health monitor, service tracker, pairing
manager) or expose the asyncio shutdown event.

Cloud relay is not among them. The status heartbeat, the pairing beacon
and the command poll are served by the native ``ados-cloud`` service,
which is the only cloud transport; this process spawns nothing for them.
"""

from __future__ import annotations

import asyncio
import time
from collections import deque
from typing import Any

from ados import __version__
from ados.core.config import ADOSConfig
from ados.core.health import HealthMonitor
from ados.core.logging import get_logger
from ados.core.service_tracker import ServiceState, ServiceTracker

from .service_registry import register_services

log = get_logger("main")


class AgentApp:
    """Main application orchestrator. Runs all services as asyncio tasks."""

    def __init__(self, config: ADOSConfig, demo: bool = False) -> None:
        self.config = config
        self.demo = demo
        self.health = HealthMonitor()
        self.services = ServiceTracker()
        self._shutdown = asyncio.Event()
        self._start_time = time.monotonic()
        self._tasks: list[asyncio.Task] = []

        # CPU/memory history ring buffers for sparkline charts (5s interval, 60 samples = 5 min)
        self._cpu_history: deque[float] = deque(maxlen=60)
        self._memory_history: deque[float] = deque(maxlen=60)

        # Lazily-initialized service references (private — internal only).
        # The FC/vehicle/param handles are read-only shims over the native
        # router's state IPC snapshot, set up in register_services; the
        # state client owns the subscription to /run/ados/state.sock.
        self._fc_connection = None
        self._vehicle_state = None
        self._param_cache = None
        self._state_client: Any = None
        self._video_pipeline: Any = None
        self._wfb_manager: Any = None
        # Tracks the last `mavlinkWsUrl` we emitted on a heartbeat so we
        # can surface the previous value as `mavlinkWsUrlPrev` for one tick
        # whenever the URL rotates (e.g. config reload, tunnel re-issue).
        self._last_mavlink_ws_url: str | None = None
        # Vision model registry + cache, populated by service_registry.
        self.model_manager: Any = None

        # Pairing and discovery (public — accessed by API routes)
        from ados.core.pairing import PairingManager
        self.pairing_manager = PairingManager(state_path=config.pairing.state_path)
        self.discovery_service: Any = None
        self.board_name = "unknown"

    @property
    def uptime_seconds(self) -> float:
        return time.monotonic() - self._start_time

    async def start(self) -> None:
        """Start all agent services and block until shutdown is requested."""
        await register_services(self)

        # Notify systemd
        self.health.sd_notify_ready()

        log.info("agent_started", services=len(self._tasks))

        # Wait for shutdown signal
        await self._shutdown.wait()
        await self._stop()

    def _start_service(self, name: str, coro) -> None:
        """Create a tracked asyncio task for a service."""
        self.services.set_state(name, ServiceState.STARTING)

        async def _wrapper():
            try:
                self.services.set_state(name, ServiceState.RUNNING)
                await coro
            except asyncio.CancelledError:
                self.services.set_state(name, ServiceState.STOPPED)
                raise
            except Exception as exc:
                log.error("service_failed", service=name, error=str(exc))
                self.services.set_state(name, ServiceState.FAILED)

        task = asyncio.create_task(_wrapper(), name=name)
        self._tasks.append(task)


    async def _health_loop(self) -> None:
        """Periodically check system health."""
        while not self._shutdown.is_set():
            self.health.check_system()
            self.health.sd_notify_watchdog()
            await asyncio.sleep(5)


    async def _stop(self) -> None:
        """Gracefully stop all services."""
        log.info("agent_stopping")

        # Unregister mDNS before cancelling tasks
        if self.discovery_service:
            await self.discovery_service.unregister()

        for task in self._tasks:
            name = task.get_name()
            self.services.set_state(name, ServiceState.STOPPED)
            task.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)
        log.info("agent_stopped")

    def request_shutdown(self) -> None:
        """Signal the agent to shut down."""
        self._shutdown.set()


# Re-export so the legacy `from ados.core.main import __version__` path
# (used by some setup-facade callers) keeps resolving.
__all__ = ["AgentApp", "__version__"]
