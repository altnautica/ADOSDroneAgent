"""Cross-process mesh-event tailer.

Bridges the cross-process mesh-event journal (`/run/ados/mesh-events.jsonl`)
back onto the in-process asyncio `MeshEventBus` that the REST WebSocket
(`/ws/mesh`) and the OLED screens already subscribe to.

When the relay/receiver loops run as their own process (the native
data-plane binary invoked with `--role relay|receiver`), they cannot publish
onto the API process's in-memory bus. Instead they append one JSON object per
line to the journal. This tailer follows the file and republishes each line so
the existing consumers see `relay_connected` / `relay_disconnected` /
`receiver_unreachable` / `wfb_adapter_missing` exactly as if a same-process
manager had published them.

Line shape (matching the in-process `MeshEvent` envelope):

    {"bus": "mesh", "kind": "relay_connected", "timestamp_ms": 123,
     "payload": {...}}

Best-effort telemetry: a malformed line is skipped, a missing file is polled
for, and the tailer seeks to end on start so a long-lived journal never
replays stale events into a freshly connected GCS.
"""

from __future__ import annotations

import asyncio
import json
import time
from pathlib import Path
from typing import Any

from ados.core.logging import get_logger
from ados.core.paths import MESH_EVENTS_JSONL

from .events import MeshEvent, get_mesh_event_bus

log = get_logger("ground_station.mesh_event_tailer")

# How long to sleep between polls when no new line is available or the journal
# does not exist yet. The journal is append-only and low-rate, so a short poll
# keeps latency low without busy-waiting.
_POLL_INTERVAL_S = 0.5

# The kinds the relay/receiver loops emit across the seam. A line carrying any
# other kind is still republished (forward-compatible) but these are the ones
# this seam exists for.
_KNOWN_KINDS = {
    "relay_connected",
    "relay_disconnected",
    "receiver_unreachable",
    "wfb_adapter_missing",
}


def _parse_line(line: str) -> MeshEvent | None:
    """Parse one journal line into a `MeshEvent`, or `None` if malformed."""
    line = line.strip()
    if not line:
        return None
    try:
        obj: dict[str, Any] = json.loads(line)
    except (json.JSONDecodeError, ValueError):
        return None
    kind = obj.get("kind")
    if not isinstance(kind, str):
        return None
    timestamp_ms = obj.get("timestamp_ms")
    if not isinstance(timestamp_ms, int):
        timestamp_ms = int(time.time() * 1000)
    payload = obj.get("payload")
    if not isinstance(payload, dict):
        payload = {}
    return MeshEvent(kind=kind, timestamp_ms=timestamp_ms, payload=payload)


async def tail_mesh_events(
    path: Path = MESH_EVENTS_JSONL,
    stop: asyncio.Event | None = None,
) -> None:
    """Follow the mesh-event journal and republish each line onto the bus.

    Runs until `stop` is set (or forever when `stop` is None). Seeks to the
    current end of the file on first open so only events written after the
    tailer starts are republished. Re-opens the file if it is truncated or
    recreated (tmpfs wipe on a service restart).
    """
    bus = get_mesh_event_bus()
    handle = None
    try:
        while stop is None or not stop.is_set():
            if handle is None:
                if not path.exists():
                    await _sleep_or_stop(stop)
                    continue
                try:
                    handle = path.open("r", encoding="utf-8")
                    handle.seek(0, 2)  # seek to end: skip the backlog
                except OSError as exc:
                    log.debug("mesh_event_journal_open_failed", error=str(exc))
                    await _sleep_or_stop(stop)
                    continue

            line = handle.readline()
            if not line:
                # Detect truncation/recreation: if the file shrank below our
                # offset, reopen from the new end.
                try:
                    if path.stat().st_size < handle.tell():
                        handle.close()
                        handle = None
                        continue
                except OSError:
                    handle.close()
                    handle = None
                    continue
                await _sleep_or_stop(stop)
                continue

            event = _parse_line(line)
            if event is None:
                continue
            if event.kind not in _KNOWN_KINDS:
                log.debug("mesh_event_unknown_kind", kind=event.kind)
            try:
                await bus.publish(event)
            except Exception as exc:  # bus closed under us; keep tailing
                log.debug("mesh_event_republish_failed", error=str(exc))
    finally:
        if handle is not None:
            handle.close()


async def _sleep_or_stop(stop: asyncio.Event | None) -> None:
    """Sleep one poll interval, or return early if `stop` fires."""
    if stop is None:
        await asyncio.sleep(_POLL_INTERVAL_S)
        return
    try:
        await asyncio.wait_for(stop.wait(), timeout=_POLL_INTERVAL_S)
    except TimeoutError:
        pass
