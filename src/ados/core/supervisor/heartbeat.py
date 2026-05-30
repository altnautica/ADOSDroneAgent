"""Heartbeat reporting mixin: per-service status + full heartbeat payload."""

from __future__ import annotations

from typing import Any

# The radio-block builder + WFB status fetch helpers live in a small neutral
# library module so any payload builder can pull them without importing the
# supervisor. Re-exported here for the in-process status path below.
from ados.core.radio_block import (
    build_radio_block,
    fetch_wfb_status_via_http,
)

__all__ = ["build_radio_block", "fetch_wfb_status_via_http", "HeartbeatMixin"]


class HeartbeatMixin:
    """Status reporting for the Supervisor."""

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

    def _get_radio_block(self) -> dict[str, Any]:
        """Pull a `WfbManager.get_status()` view, falling back to localhost.

        The supervisor itself does not own a WfbManager (the agent
        process does). When unavailable, we ask the agent's REST surface
        on localhost; on any failure we emit an `absent` block so the
        GCS can render a neutral state.
        """
        wfb = getattr(self, "_wfb_manager", None)
        status: dict[str, Any] | None = None
        if wfb is not None:
            try:
                status = wfb.get_status()
            except Exception:
                status = None
        if status is None:
            status = fetch_wfb_status_via_http()
        return build_radio_block(status)

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

        # Setup state and profile source. Surfaced so the GCS fleet view
        # can show an "auto-configured" pill on cards whose profile was
        # picked by the boot-time detect rather than the operator. Both
        # fields are optional: the GCS handles a heartbeat that lacks
        # them gracefully.
        setup_state = "configured"
        profile_source: str | None = None
        try:
            cfg = getattr(self, "config", None)
            agent_profile = (
                str(getattr(cfg.agent, "profile", "") or "") if cfg else ""
            )
            explicit = agent_profile in ("drone", "ground_station")
            if explicit:
                profile_source = "user"
            else:
                from pathlib import Path

                from ados.core.paths import PROFILE_CONF

                if Path(str(PROFILE_CONF)).is_file():
                    try:
                        import yaml

                        data = yaml.safe_load(
                            Path(str(PROFILE_CONF)).read_text(encoding="utf-8")
                        )
                        if isinstance(data, dict):
                            src = data.get("source")
                            if src in ("detected", "tiebreaker", "override", "default"):
                                profile_source = src
                    except Exception:
                        profile_source = None
        except Exception:
            setup_state = "configured"
            profile_source = None

        # Sum process-level metrics
        total_cpu = sum(s.cpu_percent for s in self._services.values())
        total_mem = sum(s.memory_mb for s in self._services.values())

        # Video pipeline restart counter, defensively read.
        vp = getattr(self, "_video_pipeline", None)
        try:
            video_restart_attempts = (
                int(vp.restart_attempts()) if vp is not None else 0
            )
        except Exception:
            video_restart_attempts = 0

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
            "videoRestartAttempts": video_restart_attempts,
            "setupState": setup_state,
            "profileSource": profile_source,
            # Forward-compatible radio link block; older GCS ignore it.
            "radio": self._get_radio_block(),
        }
