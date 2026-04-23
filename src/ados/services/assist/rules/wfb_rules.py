"""WFB-ng link quality rules."""

from __future__ import annotations

from typing import TYPE_CHECKING

from .base import Rule, Suggestion

if TYPE_CHECKING:
    from ..correlator import ContextWindow


class WfbFecBelowFloor(Rule):
    id = "wfb.fec_below_floor"
    safety_class = "read"
    summary = "WFB-ng FEC recovery rate below floor"

    def match(self, ctx: "ContextWindow") -> Suggestion | None:
        wfb_events = [
            e for e in ctx.events_since(ctx._events[0].ts if ctx._events else 0)
            if e.source == "wfb"
        ]
        if not wfb_events:
            return None
        latest = wfb_events[-1]
        fec_rec = latest.fields.get("fec_rec", 0)
        total = latest.fields.get("packets_total", 1) or 1
        fec_rate = fec_rec / total
        if fec_rate > 0.1:  # More than 10% FEC recovery
            return self._suggestion(
                f"WFB-ng FEC recovery rate is {fec_rate*100:.0f}% — link may be degraded",
                confidence=min(0.9, fec_rate * 5),
                evidence={"fec_rate": fec_rate, "last_stats": latest.fields},
            )
        return None


class WfbRssiDrop(Rule):
    id = "wfb.rssi_drop"
    safety_class = "read"
    summary = "WFB-ng RSSI dropped significantly"

    def match(self, ctx: "ContextWindow") -> Suggestion | None:
        wfb_events = [e for e in ctx._events if e.source == "wfb"]
        if len(wfb_events) < 5:
            return None
        recent = [e.fields.get("rssi", 0) for e in wfb_events[-5:]]
        older = [e.fields.get("rssi", 0) for e in wfb_events[-10:-5]]
        if not recent or not older:
            return None
        avg_recent = sum(recent) / len(recent)
        avg_older = sum(older) / len(older)
        drop = avg_older - avg_recent
        if drop > 10:  # More than 10 dBm drop
            return self._suggestion(
                f"WFB-ng RSSI dropped {drop:.0f} dBm — check antenna orientation",
                confidence=min(0.85, drop / 30),
                evidence={"rssi_drop_db": drop, "current_rssi": avg_recent},
            )
        return None


class WfbLinkBudget(Rule):
    id = "wfb.link_budget"
    safety_class = "read"
    summary = "WFB-ng link budget margin thin"

    def match(self, ctx: "ContextWindow") -> Suggestion | None:
        wfb_events = [e for e in ctx._events if e.source == "wfb"]
        if not wfb_events:
            return None
        latest = wfb_events[-1]
        rssi = latest.fields.get("rssi", -50)
        if rssi < -90:
            return self._suggestion(
                f"WFB-ng RSSI is {rssi} dBm — link budget margin thin, consider reducing range",
                confidence=0.8,
                evidence={"rssi": rssi},
            )
        return None
