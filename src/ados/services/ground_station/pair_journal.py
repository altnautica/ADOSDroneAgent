"""Cross-process field-pairing event journal seam.

The in-process pairing bus (`PairingEventBus`) lives in the API process; the
native control surface that serves the mesh event stream is a separate process
and cannot reach that bus. So every pair event published onto the bus is also
mirrored as one newline-JSON line to a journal the native handler tails,
alongside the mesh-event journal the native data-plane writes.

Both the receiver-side accept-window state machine (`pairing_manager`) and the
relay-side join client (`pairing_client`) publish onto the same pair bus, so
both route their publishes through `publish_pair_event` here. Keeping the seam
in one module means the native handler sees the whole pair bus, exactly as the
prior single in-process WebSocket subscription did.

Line shape (matching the mesh-event journal envelope so the native tailer fans
both buses into one socket):

    {"bus": "pair", "kind": "join_approved", "timestamp_ms": 123,
     "payload": {...}}

Append-only and best-effort: a journal write must never break pairing, so any
I/O error is logged and dropped. Bounded by the tmpfs wipe on reboot, the same
way the mesh-event journal is.
"""

from __future__ import annotations

import json

from ados.core.logging import get_logger
from ados.core.paths import PAIR_EVENTS_JSONL

from .events import PairingEvent, PairingEventBus

log = get_logger("ground_station.pair_journal")


def journal_pair_event(event: PairingEvent) -> None:
    """Mirror one pair event into the cross-process journal.

    Append-only and best-effort: any I/O error is logged at debug and dropped
    so a journal write can never break the pairing flow.
    """
    line = json.dumps(
        {
            "bus": "pair",
            "kind": event.kind,
            "timestamp_ms": event.timestamp_ms,
            "payload": event.payload,
        }
    )
    try:
        PAIR_EVENTS_JSONL.parent.mkdir(parents=True, exist_ok=True)
        with PAIR_EVENTS_JSONL.open("a", encoding="utf-8") as handle:
            handle.write(line + "\n")
    except OSError as exc:
        log.debug("pair_event_journal_write_failed", kind=event.kind, error=str(exc))


async def publish_pair_event(bus: PairingEventBus, event: PairingEvent) -> None:
    """Publish a pair event onto the in-process bus and mirror it to the
    cross-process journal, so both the same-process consumers (OLED) and the
    out-of-process native handler see the event. Journal-then-publish."""
    journal_pair_event(event)
    await bus.publish(event)
