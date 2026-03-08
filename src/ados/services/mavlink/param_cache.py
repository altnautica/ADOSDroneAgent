"""Persistent parameter cache for FC parameters."""

from __future__ import annotations

import json
import time
from dataclasses import dataclass
from pathlib import Path

from ados.core.logging import get_logger

log = get_logger("mavlink.param_cache")

DEFAULT_CACHE_PATH = "/var/lib/ados/params.json"


@dataclass
class ParamEntry:
    value: float
    param_type: int
    last_updated: float


class ParamCache:
    """In-memory parameter cache with optional JSON file persistence.

    Provides fast get/set access to FC parameters and persists them to disk
    so they survive agent restarts.
    """

    def __init__(self, path: str | Path = DEFAULT_CACHE_PATH) -> None:
        self._path = Path(path)
        self._params: dict[str, ParamEntry] = {}

    @property
    def count(self) -> int:
        return len(self._params)

    def get(self, name: str) -> float | None:
        """Get a cached parameter value by name. Returns None if not cached."""
        entry = self._params.get(name)
        if entry is not None:
            return entry.value
        return None

    def set(self, name: str, value: float, param_type: int = 0) -> None:
        """Set a parameter value in the cache."""
        self._params[name] = ParamEntry(
            value=value,
            param_type=param_type,
            last_updated=time.time(),
        )

    def get_all(self) -> dict[str, float]:
        """Return all cached parameter names and values."""
        return {name: entry.value for name, entry in self._params.items()}

    def get_all_detailed(self) -> dict[str, dict]:
        """Return all cached parameters with metadata."""
        return {
            name: {
                "value": entry.value,
                "param_type": entry.param_type,
                "last_updated": entry.last_updated,
            }
            for name, entry in self._params.items()
        }

    def clear(self) -> None:
        """Remove all cached parameters."""
        self._params.clear()

    def save(self) -> None:
        """Persist the cache to disk as JSON."""
        try:
            self._path.parent.mkdir(parents=True, exist_ok=True)
            data = {
                name: {
                    "value": entry.value,
                    "param_type": entry.param_type,
                    "last_updated": entry.last_updated,
                }
                for name, entry in self._params.items()
            }
            tmp_path = self._path.with_suffix(".tmp")
            tmp_path.write_text(json.dumps(data, indent=2))
            tmp_path.replace(self._path)
            log.info("param_cache_saved", count=len(data), path=str(self._path))
        except OSError as e:
            log.warning("param_cache_save_failed", error=str(e), path=str(self._path))

    def load(self) -> None:
        """Load the cache from disk. Silently skips if file doesn't exist."""
        if not self._path.is_file():
            log.debug("param_cache_no_file", path=str(self._path))
            return

        try:
            raw = json.loads(self._path.read_text())
            for name, entry_data in raw.items():
                self._params[name] = ParamEntry(
                    value=float(entry_data["value"]),
                    param_type=int(entry_data.get("param_type", 0)),
                    last_updated=float(entry_data.get("last_updated", 0)),
                )
            log.info("param_cache_loaded", count=len(self._params), path=str(self._path))
        except (json.JSONDecodeError, KeyError, ValueError, OSError) as e:
            log.warning("param_cache_load_failed", error=str(e), path=str(self._path))
