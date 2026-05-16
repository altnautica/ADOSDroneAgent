"""Runtime context passed to the cloud-relay loops.

A small dataclass that bundles the shared state every loop reads:
the loaded config, the structured logger, the shared shutdown event,
the pairing manager, the effective Convex URL (empty when the agent
opted out of the cloud relay), and the per-process metric ring
buffers used by the heartbeat composer.
"""

from __future__ import annotations

from collections import deque
from dataclasses import dataclass, field
from typing import Any


@dataclass
class CloudContext:
    """Bundle of state the cloud-relay loops share.

    The dataclass owns nothing; it borrows references to the live
    instances the ``main()`` entry built. Each loop reads what it
    needs from the same instance.
    """

    config: Any
    log: Any
    shutdown: Any  # asyncio.Event — typed loosely to avoid import-time asyncio
    pairing: Any
    convex_url: str
    board: Any
    start_time: float
    vehicle_state: Any
    cpu_history: deque = field(default_factory=lambda: deque(maxlen=60))
    memory_history: deque = field(default_factory=lambda: deque(maxlen=60))


__all__ = ["CloudContext"]
