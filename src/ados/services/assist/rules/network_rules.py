from .base import Rule, Suggestion
from typing import TYPE_CHECKING
if TYPE_CHECKING:
    from ..correlator import ContextWindow

class NetworkModemLost(Rule):
    id = "network.modem_lost"
    summary = "4G/5G modem connection lost"
    def match(self, ctx):
        modem = [e for e in ctx._events if e.source == "service_state" and "modem" in e.fields.get("service", "")]
        if modem and any(e.severity in ("error", "warning") for e in modem[-3:]):
            return self._suggestion("4G modem connection degraded — cloud relay may disconnect", 0.7, {"events": len(modem)})
        return None
