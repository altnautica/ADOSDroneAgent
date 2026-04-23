"""Event correlator for the Assist service.

Maintains a rolling context window (default 10 minutes) of AssistEvents
from all 10 collectors. Groups events by correlation_tag and links
related events into causal chains using pattern matching + time windows.

The correlator does NOT run a language model. It is pattern-matching
plus time windowing. Goal: evidence grouping, not inference.

Output: a current context window, serializable as a JSON Snapshot.
"""

from __future__ import annotations

import time
from collections import defaultdict
from dataclasses import dataclass, field
from typing import Any

import structlog

log = structlog.get_logger()

DEFAULT_WINDOW_MINUTES = 10
MAX_WINDOW_MINUTES = 60


@dataclass
class AssistEvent:
    kind: str
    source: str
    ts: float
    severity: str  # debug | info | warning | error | critical
    fields: dict[str, Any]
    correlation_tags: list[str] = field(default_factory=list)


@dataclass
class CausalChain:
    """A group of related events forming a causal sequence."""
    chain_id: str
    tags: list[str]
    events: list[AssistEvent]
    started_at: float
    last_event_at: float


class ContextWindow:
    """Rolling time-window of correlated events."""

    def __init__(self, window_minutes: int = DEFAULT_WINDOW_MINUTES) -> None:
        self._window_s = min(window_minutes, MAX_WINDOW_MINUTES) * 60
        self._events: list[AssistEvent] = []
        self._chains: dict[str, CausalChain] = {}
        self._drop_count = 0
        self._max_events = 2048

    def push(self, event: AssistEvent) -> None:
        """Add an event to the window. Evicts oldest if full."""
        now = time.time()
        # Evict expired events
        self._events = [e for e in self._events if (now - e.ts) < self._window_s]

        if len(self._events) >= self._max_events:
            self._events = self._events[len(self._events) // 4:]
            self._drop_count += 1

        self._events.append(event)
        self._update_chains(event)

    def _update_chains(self, event: AssistEvent) -> None:
        for tag in event.correlation_tags:
            if tag in self._chains:
                chain = self._chains[tag]
                chain.events.append(event)
                chain.last_event_at = event.ts
            else:
                import secrets
                self._chains[tag] = CausalChain(
                    chain_id=secrets.token_hex(4),
                    tags=[tag],
                    events=[event],
                    started_at=event.ts,
                    last_event_at=event.ts,
                )

    def snapshot(self) -> dict[str, Any]:
        """Serialize the current context window as a JSON-compatible dict."""
        now = time.time()
        return {
            "ts": now,
            "window_seconds": self._window_s,
            "event_count": len(self._events),
            "drop_count": self._drop_count,
            "chains": [
                {
                    "chain_id": c.chain_id,
                    "tags": c.tags,
                    "event_count": len(c.events),
                    "started_at": c.started_at,
                    "last_event_at": c.last_event_at,
                    "recent_events": [
                        {
                            "kind": e.kind,
                            "source": e.source,
                            "ts": e.ts,
                            "severity": e.severity,
                            "fields": e.fields,
                        }
                        for e in c.events[-5:]  # Last 5 per chain
                    ],
                }
                for c in sorted(
                    self._chains.values(),
                    key=lambda c: c.last_event_at,
                    reverse=True,
                )[:20]  # Top 20 most recent chains
            ],
        }

    def events_since(self, ts: float) -> list[AssistEvent]:
        return [e for e in self._events if e.ts >= ts]

    @property
    def drop_rate(self) -> int:
        return self._drop_count
