"""AgentFetcher — centralized HTTP client with caching and ring buffers."""

from __future__ import annotations

from collections import deque
from typing import Any

import httpx
import structlog

log = structlog.get_logger("tui.fetcher")


class AgentFetcher:
    """Centralized data fetcher for the ADOS TUI.

    Provides:
    - Single ``httpx.AsyncClient`` with connection pooling
    - Per-endpoint response caching
    - Ring buffers for sparkline history data
    """

    def __init__(self, base_url: str, maxlen: int = 60) -> None:
        self._base_url = base_url
        self._client = httpx.AsyncClient(
            base_url=base_url,
            timeout=3.0,
        )
        self._cache: dict[str, dict[str, Any]] = {}

        # Ring buffers for sparkline data
        self.altitude_history: deque[float] = deque(maxlen=maxlen)
        self.speed_history: deque[float] = deque(maxlen=maxlen)
        self.rssi_history: deque[float] = deque(maxlen=maxlen)
        self.bitrate_history: deque[float] = deque(maxlen=maxlen)
        self.cpu_history: deque[float] = deque(maxlen=maxlen)
        self.ram_history: deque[float] = deque(maxlen=maxlen)
        self.climb_history: deque[float] = deque(maxlen=maxlen)

        self._buffers: dict[str, deque[float]] = {
            "altitude": self.altitude_history,
            "speed": self.speed_history,
            "rssi": self.rssi_history,
            "bitrate": self.bitrate_history,
            "cpu": self.cpu_history,
            "ram": self.ram_history,
            "climb": self.climb_history,
        }

    def push_sample(self, name: str, value: float) -> None:
        """Append a value to the named ring buffer."""
        buf = self._buffers.get(name)
        if buf is not None:
            buf.append(value)

    async def _get(self, path: str) -> dict[str, Any] | None:
        """Fetch a JSON endpoint, cache on success, return None on error."""
        try:
            resp = await self._client.get(path)
            data = resp.json()
            self._cache[path] = data
            return data
        except httpx.ConnectError:
            log.debug("agent_unreachable", path=path)
            return None
        except Exception as exc:
            log.warning("fetch_failed", path=path, error=str(exc))
            return None

    def get_cached(self, path: str) -> dict[str, Any] | None:
        """Return the last successful response for a path, or None."""
        return self._cache.get(path)

    async def get_status(self) -> dict[str, Any] | None:
        return await self._get("/api/status")

    async def get_telemetry(self) -> dict[str, Any] | None:
        return await self._get("/api/telemetry")

    async def get_services(self) -> dict[str, Any] | None:
        return await self._get("/api/services")

    async def get_logs(self, limit: int = 10) -> dict[str, Any] | None:
        return await self._get(f"/api/logs?limit={limit}")

    async def get_wfb(self) -> dict[str, Any] | None:
        return await self._get("/api/wfb")

    async def get_video(self) -> dict[str, Any] | None:
        return await self._get("/api/video")

    async def get_ota(self) -> dict[str, Any] | None:
        return await self._get("/api/ota")

    async def get_pairing(self) -> dict[str, Any] | None:
        return await self._get("/api/pairing/info")

    async def get_scripts(self) -> dict[str, Any] | None:
        return await self._get("/api/scripts")

    async def get_scripting_status(self) -> dict[str, Any] | None:
        return await self._get("/api/scripting/status")

    async def get_config(self) -> dict[str, Any] | None:
        return await self._get("/api/config")

    async def close(self) -> None:
        """Close the underlying HTTP client."""
        await self._client.aclose()
