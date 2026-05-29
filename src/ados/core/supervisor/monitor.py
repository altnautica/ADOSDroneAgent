"""Monitor loop mixin: periodic health check + per-PID metrics + watchdog ping."""

from __future__ import annotations

import asyncio
import os
import subprocess
import time

import structlog

log = structlog.get_logger()

# How often the monitor re-attempts a service parked in failed /
# circuit_open. A crashed hardware service (e.g. ados-video after a
# camera unplug tripped systemd's start-limit) must keep being retried
# so it self-recovers when the hardware or condition returns (Rule 26).
_PARKED_RETRY_COOLDOWN_S = 30.0


class MonitorMixin:
    """Periodic monitor loop for the Supervisor."""

    async def _monitor_loop(self) -> None:
        """Main loop: check service health, collect metrics, send heartbeat."""
        while not self._shutdown.is_set():
            # Check each service
            for name, spec in self._services.items():
                if spec.state in ("running", "starting"):
                    is_active = await self._check_service_active(name)
                    if not is_active and spec.state == "running":
                        log.warning("service_died", service=name)
                        spec.state = "failed"
                        spec.failure_times.append(time.monotonic())
                        self._check_circuit_breaker(spec)
                        # Auto-restart if circuit not open
                        if spec.state != "circuit_open":
                            log.info("service_auto_restart", service=name)
                            await self.start_service(name)
                elif (
                    spec.state in ("failed", "circuit_open")
                    and spec.enabled
                    and spec.category in ("core", "hardware")
                ):
                    # Periodically re-attempt a parked service so it
                    # self-recovers instead of staying dead until a
                    # manual restart. start_service() clears the systemd
                    # start-limit (reset-failed) and the circuit-breaker
                    # half-opens after its window, so the retry actually
                    # takes once the underlying condition clears.
                    now = time.monotonic()
                    if now - spec.last_retry_at < _PARKED_RETRY_COOLDOWN_S:
                        continue
                    spec.last_retry_at = now
                    log.info(
                        "service_parked_retry", service=name, state=spec.state
                    )
                    await self.start_service(name)

            # Collect per-PID metrics
            self._collect_metrics()

            # Ping systemd watchdog
            self._sd_notify_watchdog()

            await asyncio.sleep(5)

    async def _check_service_active(self, name: str) -> bool:
        """Check if a systemd service is active."""
        try:
            result = subprocess.run(
                ["systemctl", "is-active", "--quiet", name],
                capture_output=True,
                timeout=5,
            )
            return result.returncode == 0
        except Exception:
            return False

    def _collect_metrics(self) -> None:
        """Collect per-PID CPU/memory for all running services."""
        try:
            import psutil
        except ImportError:
            return

        for name, spec in self._services.items():
            if spec.state != "running":
                spec.pid = None
                spec.cpu_percent = 0.0
                spec.memory_mb = 0.0
                spec.uptime_seconds = 0.0
                continue

            # Get PID from systemd
            try:
                result = subprocess.run(
                    ["systemctl", "show", "-p", "MainPID", "--value", name],
                    capture_output=True,
                    text=True,
                    timeout=5,
                )
                pid = int(result.stdout.strip())
                if pid <= 0:
                    continue
                spec.pid = pid

                proc = psutil.Process(pid)
                spec.cpu_percent = proc.cpu_percent(interval=0)
                spec.memory_mb = proc.memory_info().rss / (1024 * 1024)

                create_time = proc.create_time()
                spec.uptime_seconds = time.time() - create_time
            except (psutil.NoSuchProcess, psutil.AccessDenied, ValueError, Exception):
                pass

        # Update history buffers
        try:
            cpu = psutil.cpu_percent(interval=0)
            mem = psutil.virtual_memory().percent
            self._cpu_history.append(cpu)
            self._memory_history.append(mem)
        except Exception:
            pass

    def _sd_notify_watchdog(self) -> None:
        """Ping systemd watchdog."""
        try:
            import socket

            notify_socket = os.environ.get("NOTIFY_SOCKET")
            if notify_socket:
                sock = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
                sock.connect(notify_socket)
                sock.sendall(b"WATCHDOG=1")
                sock.close()
        except Exception:
            pass
