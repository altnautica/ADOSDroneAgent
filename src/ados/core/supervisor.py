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
    # DEC-112: profile_gate scopes the service to one profile.
    # None = runs on any profile. "drone" or "ground_station" gate it.
    profile_gate: str | None = None
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
    # MSN-028 Phase 4 Track A Wave 3: Peripheral Manager plugin registry.
    # Cross-profile (no profile_gate) — peripherals exist on both drone
    # and ground-station profiles.
    {"name": "ados-peripherals", "category": "hardware"},
    # DEC-112: ground-station only services.
    {"name": "ados-wfb-rx", "category": "hardware", "profile_gate": "ground_station"},
    {"name": "ados-mediamtx-gs", "category": "hardware", "profile_gate": "ground_station"},
    {"name": "ados-usb-gadget", "category": "hardware", "profile_gate": "ground_station"},
    # MSN-025 Wave D: physical UI + AP + first-boot captive portal.
    {"name": "ados-oled", "category": "hardware", "profile_gate": "ground_station"},
    {"name": "ados-buttons", "category": "hardware", "profile_gate": "ground_station"},
    {"name": "ados-hostapd", "category": "hardware", "profile_gate": "ground_station"},
    {"name": "ados-dnsmasq-gs", "category": "hardware", "profile_gate": "ground_station"},
    {"name": "ados-setup-captive", "category": "ondemand", "profile_gate": "ground_station"},
    # MSN-026 Wave C Cellos: Phase 2 standalone flight stack.
    {"name": "ados-kiosk", "category": "hardware", "profile_gate": "ground_station"},
    {"name": "ados-input", "category": "hardware", "profile_gate": "ground_station"},
    {"name": "ados-pic", "category": "hardware", "profile_gate": "ground_station"},
    # MSN-027 Wave C Cellos: Phase 3 uplink matrix + cloud relay.
    # No `network` or `cloud` category exists in the supervisor taxonomy
    # (categories are core/hardware/suite/ondemand). Uplink managers are
    # hardware-like (they bind to real interfaces). The cloud relay is
    # treated as core because it is always running on the ground-station
    # profile, independent of hardware detection.
    {"name": "ados-uplink-router", "category": "hardware", "profile_gate": "ground_station"},
    {"name": "ados-modem", "category": "hardware", "profile_gate": "ground_station"},
    {"name": "ados-wifi-client", "category": "hardware", "profile_gate": "ground_station"},
    {"name": "ados-ethernet", "category": "hardware", "profile_gate": "ground_station"},
    {"name": "ados-cloud-relay", "category": "core", "profile_gate": "ground_station"},
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
            spec = ServiceSpec(
                name=svc_def["name"],
                category=svc_def["category"],
                profile_gate=svc_def.get("profile_gate"),
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

        # DEC-112: gate by agent profile. Services tagged for one profile
        # do not start on another.
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

        has_camera = any(Path("/dev").glob("video[0-9]*")) or self._check_csi_camera()
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


    # ── DEC-106 Bug #15: Hot-plug handling ─────────────────────

    async def _run_hotplug_monitor(self) -> None:
        """Wrapper around HotplugMonitor.run() with first-scan gating.

        The monitor's first scan fires "add" events for every USB device
        that was already present at supervisor startup. We do not want to
        cascade-restart services for those. The gate flips open after
        ~1.5× the first poll interval, which guarantees the first scan
        finished.
        """
        if self._hotplug_monitor is None:
            return

        interval = self._hotplug_monitor._interval  # noqa: SLF001
        asyncio.create_task(self._hotplug_enable_gate(interval * 1.5))

        try:
            await self._hotplug_monitor.run(self._on_hotplug_event)
        except asyncio.CancelledError:
            log.info("hotplug_monitor_task_cancelled")
            raise
        except Exception as exc:
            log.error("hotplug_monitor_crashed", error=str(exc))

    async def _hotplug_enable_gate(self, delay: float) -> None:
        """Open the hot-plug event gate after the first scan finishes."""
        await asyncio.sleep(delay)
        self._hotplug_first_scan_done = True
        known = (
            len(self._hotplug_monitor.known_devices)
            if self._hotplug_monitor
            else 0
        )
        log.info("hotplug_gate_open", known_devices=known)

    def _on_hotplug_event(self, event: str, device: UsbDevice) -> None:
        """DEC-106 Bug #15: dispatch USB add/remove events.

        CRITICAL: structlog uses `event` as the reserved positional
        argument for the log message. Using `event=...` as a kwarg in
        log.info() raises `TypeError: got multiple values for argument
        'event'`. All log calls here rename the payload kwarg to
        `action=` to avoid the collision.
        """
        # First-scan gate
        if not self._hotplug_first_scan_done:
            log.debug(
                "hotplug_event_pre_gate",
                action=event,
                device_name=device.name,
                category=device.category.value,
            )
            return

        # Debounce: coalesce rapid events on the same device (e.g.
        # SpeedyBee DFU→flight transition fires remove+add within ~1s).
        key = f"{device.vid:04x}:{device.pid:04x}"
        now = time.monotonic()
        last = self._hotplug_last_event_time.get(key, 0.0)
        if now - last < self._hotplug_debounce_secs:
            log.debug(
                "hotplug_event_debounced",
                action=event,
                device_name=device.name,
                category=device.category.value,
                delta_secs=round(now - last, 2),
            )
            self._hotplug_last_event_time[key] = now
            return
        self._hotplug_last_event_time[key] = now

        log.info(
            "hotplug_event",
            action=event,
            device_name=device.name,
            vid=f"{device.vid:04x}",
            pid=f"{device.pid:04x}",
            category=device.category.value,
        )

        # Route by device category → service restart
        affected_service: str | None = None
        if device.category == UsbCategory.CAMERA:
            affected_service = "ados-video"
        elif device.category == UsbCategory.FC:
            affected_service = "ados-mavlink"
        elif device.category == UsbCategory.RADIO:
            # Only restart ados-wfb for the WFB-ng adapter
            pid_hex = f"{device.pid:04x}".lower()
            if pid_hex in ("8812", "b812"):  # RTL8812AU/BU
                affected_service = "ados-wfb"
        # GPS / LORA / OTHER: log-only, no restart

        if affected_service and affected_service in self._services:
            log.info(
                "hotplug_triggered_restart",
                service=affected_service,
                action=event,
                device_name=device.name,
            )
            # Fire-and-forget so the poll loop is never blocked
            asyncio.create_task(
                self._hotplug_restart_service(affected_service)
            )

    async def _hotplug_restart_service(self, name: str) -> None:
        """Restart a service after a hot-plug event."""
        try:
            # Small delay so the kernel finishes device-node creation
            await asyncio.sleep(0.5)
            await self.restart_service(name)
            log.info("hotplug_service_restarted", service=name)
        except Exception as exc:
            log.error(
                "hotplug_service_restart_failed",
                service=name,
                error=str(exc),
            )


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
