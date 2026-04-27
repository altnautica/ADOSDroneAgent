"""Heartbeat reporting mixin: per-service status + full heartbeat payload."""

from __future__ import annotations


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
