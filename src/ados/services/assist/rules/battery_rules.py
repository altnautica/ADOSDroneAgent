from .base import Rule, Suggestion
from typing import TYPE_CHECKING
if TYPE_CHECKING:
    from ..correlator import ContextWindow

class BatteryLow(Rule):
    id = "battery.low"
    summary = "Battery critically low"
    def match(self, ctx):
        batt = [e for e in ctx._events if e.source == "state" and e.fields.get("battery")]
        if not batt:
            return None
        pct = batt[-1].fields.get("battery", {}).get("remaining", 100)
        if pct < 15:
            return self._suggestion(f"Battery at {pct}% — land immediately", 0.95, {"remaining_pct": pct})
        return None

class BatteryVoltageSag(Rule):
    id = "battery.voltage_sag"
    summary = "Battery voltage sagging under load"
    def match(self, ctx):
        return None  # Requires voltage history — stub for now
