"""Supervisor lifecycle: service start/stop, suite activation, monitor loop.

The Supervisor class is composed with HotplugMixin so the USB add/remove
routing lives in its own file.
"""

from __future__ import annotations

import asyncio
import os
import signal
import subprocess
import time
from collections import deque
from pathlib import Path

import structlog

from ados.core.paths import SUITES_DIR
from ados.hal.hotplug import HotplugMonitor
from ados.hal.usb import UsbDevice  # noqa: F401  (used via HotplugMixin)

from .heartbeat import HeartbeatMixin
from .hotplug import HotplugMixin
from .monitor import MonitorMixin
from .registry import (
    FAILURE_WINDOW_SECS,
    MAX_FAILURES,
    SERVICE_REGISTRY,
    ServiceSpec,
)

log = structlog.get_logger()


class Supervisor(HotplugMixin, MonitorMixin, HeartbeatMixin):
    """Process supervisor. Manages child systemd services."""

    def __init__(self, config) -> None:
        self.config = config
        self._shutdown = asyncio.Event()
        self._start_time = time.monotonic()
        self._services: dict[str, ServiceSpec] = {}
        self._active_suite: str | None = None
        self._cpu_history: deque[float] = deque(maxlen=3600)
        self._memory_history: deque[float] = deque(maxlen=3600)

        # hot-plug monitor state
        self._hotplug_monitor: HotplugMonitor | None = None
        self._hotplug_task: asyncio.Task | None = None
        self._hotplug_first_scan_done: bool = False
        self._hotplug_last_event_time: dict[str, float] = {}
        self._hotplug_debounce_secs: float = 3.0
        # Per-service restart task tracker so concurrent hot-plug events
        # for the same service coalesce. Latest event wins: any in-flight
        # restart task for the same service is cancelled before the new
        # one is scheduled. Prevents start/stop thrash when multiple
        # devices hot-plug within the kernel-settle window.
        self._hotplug_restart_tasks: dict[str, asyncio.Task] = {}

        # Build service map
        for svc_def in SERVICE_REGISTRY:
            spec = ServiceSpec(
                name=svc_def["name"],
                category=svc_def["category"],
                profile_gate=svc_def.get("profile_gate"),
                role_gate=svc_def.get("role_gate"),
            )
            self._services[spec.name] = spec

    @property
    def uptime_seconds(self) -> float:
        return time.monotonic() - self._start_time

    # ── Service Lifecycle ──────────────────────────────────────

    async def start_service(self, name: str) -> bool:
        """Start a systemd service."""
        spec = self._services.get(name)
        if not spec:
            log.warning("unknown_service", name=name)
            return False

        # gate by agent profile. Services tagged for one profile do not
        # start on another.
        active_profile = getattr(
            getattr(self.config, "agent", None), "profile", "auto"
        )
        if spec.profile_gate and active_profile != spec.profile_gate:
            log.info(
                "service_profile_gated",
                service=name,
                required=spec.profile_gate,
                active=active_profile,
            )
            return False

        # gate by ground-station role. role_gate is a pipe-separated list
        # of allowed roles. The active role comes from the on-disk sentinel
        # managed by role_manager so it stays in sync even if the Pydantic
        # config is briefly stale during a transition.
        if spec.role_gate:
            try:
                from ados.services.ground_station.role_manager import (
                    get_current_role,
                )
                active_role = get_current_role()
            except Exception as exc:
                log.warning(
                    "role_gate_check_failed",
                    service=name,
                    error=str(exc),
                    fallback_role="direct",
                )
                active_role = "direct"
            allowed = {r.strip() for r in spec.role_gate.split("|") if r.strip()}
            if active_role not in allowed:
                log.info(
                    "service_role_gated",
                    service=name,
                    required=spec.role_gate,
                    active=active_role,
                )
                return False

        # Circuit breaker check
        if spec.state == "circuit_open":
            now = time.monotonic()
            # Reset circuit after the window passes
            recent = [t for t in spec.failure_times if now - t < FAILURE_WINDOW_SECS]
            if len(recent) >= MAX_FAILURES:
                log.warning("circuit_breaker_open", service=name)
                return False
            spec.state = "stopped"
            spec.failure_times.clear()
            spec.failure_times.extend(recent)

        spec.state = "starting"
        try:
            result = await asyncio.to_thread(
                subprocess.run,
                ["systemctl", "start", name],
                capture_output=True,
                text=True,
                timeout=30,
            )
            if result.returncode == 0:
                spec.state = "running"
                log.info("service_started", service=name)
                return True
            else:
                spec.state = "failed"
                spec.failure_times.append(time.monotonic())
                log.error(
                    "service_start_failed",
                    service=name,
                    stderr=result.stderr.strip(),
                )
                self._check_circuit_breaker(spec)
                return False
        except subprocess.TimeoutExpired:
            spec.state = "failed"
            spec.failure_times.append(time.monotonic())
            log.error("service_start_timeout", service=name)
            self._check_circuit_breaker(spec)
            return False
        except Exception as exc:
            spec.state = "failed"
            log.error("service_start_error", service=name, error=str(exc))
            return False

    async def stop_service(self, name: str) -> bool:
        """Stop a systemd service."""
        spec = self._services.get(name)
        if not spec:
            return False

        try:
            result = await asyncio.to_thread(
                subprocess.run,
                ["systemctl", "stop", name],
                capture_output=True,
                text=True,
                timeout=30,
            )
            spec.state = "stopped"
            spec.pid = None
            spec.cpu_percent = 0.0
            spec.memory_mb = 0.0
            spec.uptime_seconds = 0.0
            log.info("service_stopped", service=name)
            return result.returncode == 0
        except Exception as exc:
            log.error("service_stop_error", service=name, error=str(exc))
            return False

    @staticmethod
    def _is_active(name: str) -> bool:
        """systemctl is-active probe. Returns True only on 'active'."""
        try:
            result = subprocess.run(
                ["systemctl", "is-active", name],
                capture_output=True,
                text=True,
                timeout=5,
            )
            return result.stdout.strip() == "active"
        except Exception:
            return False

    async def _wait_for_stop(
        self, names: list[str], timeout_secs: float = 5.0
    ) -> None:
        """Block until none of the named services report `is-active`.

        Polls systemctl is-active every 100ms. Returns early when all are
        down. Times out silently after `timeout_secs` so a stuck service
        cannot block the rest of shutdown indefinitely.
        """
        if not names:
            return
        deadline = time.monotonic() + timeout_secs
        while time.monotonic() < deadline:
            active_flags = await asyncio.gather(
                *(asyncio.to_thread(self._is_active, n) for n in names)
            )
            still_up = [n for n, alive in zip(names, active_flags) if alive]
            if not still_up:
                return
            await asyncio.sleep(0.1)
        leftover_flags = await asyncio.gather(
            *(asyncio.to_thread(self._is_active, n) for n in names)
        )
        leftover = [n for n, alive in zip(names, leftover_flags) if alive]
        if leftover:
            log.warning(
                "stop_wait_timeout",
                services=leftover,
                timeout_secs=timeout_secs,
            )

    async def restart_service(self, name: str) -> bool:
        """Restart a systemd service."""
        await self.stop_service(name)
        await asyncio.sleep(0.5)
        return await self.start_service(name)

    def _check_circuit_breaker(self, spec: ServiceSpec) -> None:
        """Open circuit breaker if too many failures in window."""
        now = time.monotonic()
        recent = [t for t in spec.failure_times if now - t < FAILURE_WINDOW_SECS]
        spec.failure_times.clear()
        spec.failure_times.extend(recent)
        if len(recent) >= MAX_FAILURES:
            spec.state = "circuit_open"
            log.error(
                "circuit_breaker_opened",
                service=spec.name,
                failures=len(recent),
                window_secs=FAILURE_WINDOW_SECS,
            )

    # ── Startup Sequence ───────────────────────────────────────

    def _apply_ground_station_role(self) -> None:
        """Write the role sentinel and mask/unmask units for the current role.

        Runs only when the agent profile is `ground_station`. Pulls the
        configured role from `config.ground_station.role` and hands it to
        the role_manager helper, which writes `/etc/ados/mesh/role` and
        applies the systemd unit mask/unmask state. Idempotent.
        """
        profile = getattr(
            getattr(self.config, "agent", None), "profile", "auto"
        )
        if profile != "ground_station":
            return
        configured_role = getattr(
            getattr(self.config, "ground_station", None), "role", "direct"
        )
        try:
            from ados.services.ground_station.role_manager import (
                apply_role_on_boot_sync,
            )
        except ImportError as exc:
            log.warning(
                "role_manager_import_failed",
                error=str(exc),
                fallback_role="direct",
            )
            return
        try:
            apply_role_on_boot_sync(configured_role)
            log.info("ground_station_role_applied", role=configured_role)
        except Exception as exc:
            log.warning(
                "ground_station_role_apply_failed",
                role=configured_role,
                error=str(exc),
            )

    async def start(self) -> None:
        """Full supervisor startup: core → hardware → suite → monitor."""
        log.info("supervisor_starting")

        # 0. On ground-station profile, apply the configured mesh role so
        #    the on-disk sentinel, systemd masks, and role-gate checks
        #    all agree before the hardware pass tries to start role-gated
        #    services. On drone profile this is a no-op.
        self._apply_ground_station_role()

        # 1. Start core services. Core tier services are independent of
        #    each other (hardware and suite tiers depend on core, not the
        #    reverse), so we start them concurrently. Errors in one start
        #    do not block the others; per-service failure handling lives
        #    inside start_service.
        core_names = [
            name
            for name, spec in self._services.items()
            if spec.category == "core"
        ]
        if core_names:
            results = await asyncio.gather(
                *(self.start_service(n) for n in core_names),
                return_exceptions=True,
            )
            for n, result in zip(core_names, results):
                if isinstance(result, Exception):
                    log.error(
                        "service_start_failed",
                        service=n,
                        error=str(result),
                    )

        # 2. Detect hardware and start hardware services
        await self._detect_and_start_hardware()

        # 3. Load active suite if configured
        suite = getattr(self.config, "active_suite", None)
        if suite:
            await self.activate_suite(suite)

        # 4. Notify systemd we're ready
        try:
            import socket

            sock = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
            notify_socket = os.environ.get("NOTIFY_SOCKET")
            if notify_socket:
                sock.connect(notify_socket)
                sock.sendall(b"READY=1")
                sock.close()
        except Exception:
            pass

        log.info("supervisor_ready", services=len(self._services))

        # start hot-plug monitor
        self._hotplug_monitor = HotplugMonitor()
        self._hotplug_task = asyncio.create_task(self._run_hotplug_monitor())
        log.info(
            "hotplug_monitor_wired",
            debounce_secs=self._hotplug_debounce_secs,
        )

        # 5. Enter monitor loop
        await self._monitor_loop()

    async def stop(self) -> None:
        """Graceful shutdown: stop services in dependency-aware order.

        Order matters because the API service is alive during teardown
        and answers HTTP requests against hardware services. If video
        is killed before API stops accepting requests, in-flight
        /api/video calls return 500.

        Sequence:
          1. Stop the HTTP frontend (ados-api) first so no new requests
             land on services that are about to die. Drain in-flight.
          2. Stop suite services (top of the dependency tree).
          3. Stop hardware services.
          4. Stop on-demand services.
          5. Stop the rest of the core services (mavlink, cloud, health).

        Between each tier we poll `systemctl is-active` for up to 5s to
        confirm the previous tier is actually down before tearing the
        next one. systemctl returns once it has SENT SIGTERM, not once
        the unit has stopped.
        """
        self._shutdown.set()
        log.info("supervisor_stopping")

        # cancel hot-plug monitor before stopping services
        if self._hotplug_monitor:
            self._hotplug_monitor.stop()
        if self._hotplug_task:
            self._hotplug_task.cancel()
            try:
                await self._hotplug_task
            except (asyncio.CancelledError, Exception):
                pass
            self._hotplug_task = None

        # Cancel any in-flight hot-plug-driven service restarts so they
        # do not race the shutdown stop_service calls below.
        pending_restarts = [
            t for t in self._hotplug_restart_tasks.values() if not t.done()
        ]
        for t in pending_restarts:
            t.cancel()
        if pending_restarts:
            await asyncio.gather(*pending_restarts, return_exceptions=True)
        self._hotplug_restart_tasks.clear()

        # Tier 0: HTTP frontend stops first so it stops accepting requests
        # against hardware services that are about to die.
        frontend_units: list[str] = []
        api_spec = self._services.get("ados-api")
        if api_spec and api_spec.state == "running":
            await self.stop_service("ados-api")
            frontend_units.append("ados-api")
        await self._wait_for_stop(frontend_units)

        # Tier 1-4: top-down dependency order. ados-api already stopped.
        for category in ("suite", "hardware", "ondemand", "core"):
            tier_units: list[str] = []
            for name, spec in self._services.items():
                if name == "ados-api":
                    continue
                if spec.category == category and spec.state == "running":
                    await self.stop_service(name)
                    tier_units.append(name)
            await self._wait_for_stop(tier_units)

        log.info("supervisor_stopped")

    # ── Hardware Detection ─────────────────────────────────────

    async def _detect_and_start_hardware(self) -> None:
        """Detect connected hardware and start appropriate services."""
        # Check video config before starting video service
        video_mode = getattr(self.config, "video", None)
        video_enabled = video_mode and getattr(video_mode, "mode", "disabled") != "disabled"

        has_camera = any(Path("/dev").glob("video[0-9]*")) or await asyncio.to_thread(
            self._check_csi_camera
        )
        if has_camera and video_enabled and "ados-video" in self._services:
            await self.start_service("ados-video")
        elif not video_enabled:
            log.info("video_service_skipped", reason="video.mode is disabled in config")

        # Check for WFB-ng adapter
        has_wfb = self._check_wfb_adapter()
        if has_wfb and "ados-wfb" in self._services:
            await self.start_service("ados-wfb")

    def _check_csi_camera(self) -> bool:
        """Check for CSI camera via rpicam-hello."""
        try:
            result = subprocess.run(
                ["rpicam-hello", "--list-cameras"],
                capture_output=True,
                text=True,
                timeout=5,
            )
            return "Available cameras" in result.stdout
        except Exception:
            return False

    def _check_wfb_adapter(self) -> bool:
        """Check for RTL8812EU USB adapter."""
        try:
            from ados.hal.usb import discover_usb_devices

            devices = discover_usb_devices()
            wfb_ids = {(0x0BDA, 0xA81A), (0x0BDA, 0x8812), (0x0BDA, 0x881A)}
            return any((d.vid, d.pid) in wfb_ids for d in devices)
        except Exception:
            return False

    # ── Suite Lifecycle ────────────────────────────────────────

    async def activate_suite(self, suite_id: str) -> bool:
        """Activate a suite: parse manifest, validate sensors, start services."""
        log.info("suite_activating", suite=suite_id)

        # Load manifest
        manifest_path = SUITES_DIR / f"{suite_id}.yaml"
        if not manifest_path.exists():
            # Check built-in suites
            manifest_path = Path(f"/opt/ados/suites/{suite_id}.yaml")
        if not manifest_path.exists():
            log.error("suite_not_found", suite=suite_id)
            return False

        try:
            import yaml

            with open(manifest_path) as f:
                manifest = yaml.safe_load(f)
        except Exception as exc:
            log.error("suite_manifest_error", suite=suite_id, error=str(exc))
            return False

        # Determine required services from manifest
        required = manifest.get("required_services", [])
        for svc_name in required:
            full_name = f"ados-{svc_name}" if not svc_name.startswith("ados-") else svc_name
            if full_name in self._services:
                self._services[full_name].category = "suite"
                await self.start_service(full_name)

        self._active_suite = suite_id
        log.info("suite_activated", suite=suite_id, services=required)
        return True

    async def deactivate_suite(self) -> bool:
        """Stop all suite-dependent services."""
        if not self._active_suite:
            return True

        log.info("suite_deactivating", suite=self._active_suite)
        for name, spec in self._services.items():
            if spec.category == "suite" and spec.state == "running":
                await self.stop_service(name)

        old_suite = self._active_suite
        self._active_suite = None
        log.info("suite_deactivated", suite=old_suite)
        return True

    # Monitor loop, metrics, and watchdog live in MonitorMixin.
    # Status reporting + heartbeat payload live in HeartbeatMixin.


# ── Entry Point ────────────────────────────────────────────────


async def run_supervisor(config) -> None:
    """Async supervisor entry point."""
    supervisor = Supervisor(config)

    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, lambda: asyncio.create_task(supervisor.stop()))

    await supervisor.start()


def main() -> None:
    """Sync entry point for console_scripts."""
    from ados.core.config import load_config
    from ados.core.logging import configure_logging

    config = load_config()
    configure_logging(config.logging.level)
    asyncio.run(run_supervisor(config))
