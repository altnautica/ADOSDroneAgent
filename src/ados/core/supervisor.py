"""ADOS Process Supervisor — manages child systemd services.

The supervisor is the master process manager for the companion board.
It starts/stops/restarts services, monitors health, collects per-PID metrics,
and drives suite-based lifecycle (activate suite → start required services).

Architecture:
  systemd → ados-supervisor.service (this)
    ├── ados-mavlink.service (core, always running)
    ├── ados-api.service (core, always running)
    ├── ados-cloud.service (core if paired)
    ├── ados-health.service (core, always running)
    ├── ados-video.service (hardware-dependent)
    ├── ados-wfb.service (hardware-dependent)
    ├── ados-scripting.service (suite-dependent)
    └── ...
"""

from __future__ import annotations

import asyncio
import os
import signal
import subprocess
import time
from collections import deque
from dataclasses import dataclass, field
from pathlib import Path

import structlog

# DEC-106 Bug #15: hot-plug handling
from ados.hal.hotplug import HotplugMonitor
from ados.hal.usb import UsbCategory, UsbDevice

log = structlog.get_logger()

# Circuit breaker: stop restarting after N failures in M seconds
MAX_FAILURES = 5
FAILURE_WINDOW_SECS = 60.0


@dataclass
class ServiceSpec:
    """Defines a managed service."""

    name: str
    category: str  # "core", "hardware", "suite", "ondemand"
    enabled: bool = True
    # Track failures for circuit breaker
    failure_times: list[float] = field(default_factory=list)
    # Runtime state
    pid: int | None = None
    cpu_percent: float = 0.0
    memory_mb: float = 0.0
    uptime_seconds: float = 0.0
    state: str = "stopped"  # stopped, starting, running, failed, circuit_open


# All services the supervisor knows about
SERVICE_REGISTRY: list[dict] = [
    # Core (always running)
    {"name": "ados-mavlink", "category": "core"},
    {"name": "ados-api", "category": "core"},
    {"name": "ados-cloud", "category": "core"},
    {"name": "ados-health", "category": "core"},
    # Hardware-dependent (started on detection)
    {"name": "ados-video", "category": "hardware"},
    {"name": "ados-wfb", "category": "hardware"},
    # Suite-dependent (started on suite activation)
    {"name": "ados-scripting", "category": "suite"},
    # On-demand
    {"name": "ados-ota", "category": "ondemand"},
    {"name": "ados-discovery", "category": "ondemand"},
]


class Supervisor:
    """Process supervisor — manages child systemd services."""

    def __init__(self, config) -> None:
        self.config = config
        self._shutdown = asyncio.Event()
        self._start_time = time.monotonic()
        self._services: dict[str, ServiceSpec] = {}
        self._active_suite: str | None = None
        self._cpu_history: deque[float] = deque(maxlen=60)
        self._memory_history: deque[float] = deque(maxlen=60)

        # DEC-106 Bug #15: hot-plug monitor state
        self._hotplug_monitor: HotplugMonitor | None = None
        self._hotplug_task: asyncio.Task | None = None
        self._hotplug_first_scan_done: bool = False
        self._hotplug_last_event_time: dict[str, float] = {}
        self._hotplug_debounce_secs: float = 3.0

        # Build service map
        for svc_def in SERVICE_REGISTRY:
            spec = ServiceSpec(name=svc_def["name"], category=svc_def["category"])
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

        # Circuit breaker check
        if spec.state == "circuit_open":
            now = time.monotonic()
            # Reset circuit after the window passes
            recent = [t for t in spec.failure_times if now - t < FAILURE_WINDOW_SECS]
            if len(recent) >= MAX_FAILURES:
                log.warning("circuit_breaker_open", service=name)
                return False
            spec.state = "stopped"
            spec.failure_times = recent

        spec.state = "starting"
        try:
            result = subprocess.run(
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
            result = subprocess.run(
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

    async def restart_service(self, name: str) -> bool:
        """Restart a systemd service."""
        await self.stop_service(name)
        await asyncio.sleep(0.5)
        return await self.start_service(name)

    def _check_circuit_breaker(self, spec: ServiceSpec) -> None:
        """Open circuit breaker if too many failures in window."""
        now = time.monotonic()
        recent = [t for t in spec.failure_times if now - t < FAILURE_WINDOW_SECS]
        spec.failure_times = recent
        if len(recent) >= MAX_FAILURES:
            spec.state = "circuit_open"
            log.error(
                "circuit_breaker_opened",
                service=spec.name,
                failures=len(recent),
                window_secs=FAILURE_WINDOW_SECS,
            )

    # ── Startup Sequence ───────────────────────────────────────

    async def start(self) -> None:
        """Full supervisor startup: core → hardware → suite → monitor."""
        log.info("supervisor_starting")

        # 1. Start core services
        for name, spec in self._services.items():
            if spec.category == "core":
                await self.start_service(name)
                await asyncio.sleep(0.5)  # stagger startup

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

        # DEC-106 Bug #15: start hot-plug monitor
        self._hotplug_monitor = HotplugMonitor()
        self._hotplug_task = asyncio.create_task(self._run_hotplug_monitor())
        log.info(
            "hotplug_monitor_wired",
            debounce_secs=self._hotplug_debounce_secs,
        )

        # 5. Enter monitor loop
        await self._monitor_loop()

    async def stop(self) -> None:
        """Graceful shutdown: stop all services in reverse order."""
        self._shutdown.set()
        log.info("supervisor_stopping")

        # DEC-106 Bug #15: cancel hot-plug monitor before stopping services
        if self._hotplug_monitor:
            self._hotplug_monitor.stop()
        if self._hotplug_task:
            self._hotplug_task.cancel()
            try:
                await self._hotplug_task
            except (asyncio.CancelledError, Exception):
                pass
            self._hotplug_task = None

        # Stop suite services first, then hardware, then core
        for category in ("suite", "hardware", "ondemand", "core"):
            for name, spec in self._services.items():
                if spec.category == category and spec.state == "running":
                    await self.stop_service(name)

        log.info("supervisor_stopped")

    # ── Hardware Detection ─────────────────────────────────────

    async def _detect_and_start_hardware(self) -> None:
        """Detect connected hardware and start appropriate services."""
        # Check video config before starting video service
        video_mode = getattr(self.config, "video", None)
        video_enabled = video_mode and getattr(video_mode, "mode", "disabled") != "disabled"

        has_camera = Path("/dev/video0").exists() or self._check_csi_camera()
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
        manifest_path = Path(f"/etc/ados/suites/{suite_id}.yaml")
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

    # ── Monitor Loop ───────────────────────────────────────────

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

    # ── Status Reporting ───────────────────────────────────────

    def get_services_status(self) -> list[dict]:
        """Get status of all services for cloud heartbeat / API."""
        result = []
        for name, spec in self._services.items():
            result.append({
                "name": name,
                "status": spec.state,
                "category": spec.category,
                "pid": spec.pid,
                "cpuPercent": round(spec.cpu_percent, 1),
                "memoryMb": round(spec.memory_mb, 1),
                "uptimeSeconds": round(spec.uptime_seconds),
            })
        return result

    def get_heartbeat_payload(self) -> dict:
        """Build full heartbeat payload for cloud status push."""
        try:
            import psutil

            vm = psutil.virtual_memory()
            disk = psutil.disk_usage("/")
            cpu_percent = psutil.cpu_percent(interval=0)
            mem_percent = vm.percent
            disk_percent = disk.percent
            temp = None
            temps = psutil.sensors_temperatures()
            for key in ("cpu_thermal", "cpu-thermal", "coretemp"):
                if key in temps and temps[key]:
                    temp = temps[key][0].current
                    break
        except Exception:
            cpu_percent = 0.0
            mem_percent = 0.0
            disk_percent = 0.0
            vm = None
            disk = None
            temp = None

        from ados import __version__
        from ados.hal.detect import detect_board

        board = detect_board()

        # Sum process-level metrics
        total_cpu = sum(s.cpu_percent for s in self._services.values())
        total_mem = sum(s.memory_mb for s in self._services.values())

        return {
            "version": __version__,
            "uptimeSeconds": self.uptime_seconds,
            "boardName": board.name if board else "unknown",
            "boardTier": board.tier if board else 0,
            "boardSoc": board.soc if board else "",
            "boardArch": board.arch if board else "",
            "cpuPercent": cpu_percent,
            "memoryPercent": mem_percent,
            "diskPercent": disk_percent,
            "temperature": temp,
            "memoryUsedMb": round(vm.used / (1024 * 1024)) if vm else 0,
            "memoryTotalMb": round(vm.total / (1024 * 1024)) if vm else 0,
            "diskUsedGb": round(disk.used / (1024**3), 1) if disk else 0,
            "diskTotalGb": round(disk.total / (1024**3), 1) if disk else 0,
            "cpuCores": psutil.cpu_count() if "psutil" in dir() else 0,
            "boardRamMb": round(vm.total / (1024 * 1024)) if vm else 0,
            "processCpuPercent": round(total_cpu, 1),
            "processMemoryMb": round(total_mem, 1),
            "cpuHistory": list(self._cpu_history),
            "memoryHistory": list(self._memory_history),
            "services": self.get_services_status(),
            "activeSuite": self._active_suite,
        }


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
