from .base import Rule, Suggestion
from typing import TYPE_CHECKING
if TYPE_CHECKING:
    from ..correlator import ContextWindow

class ServiceCrashLoop(Rule):
    id = "service.crash_loop"
    summary = "Agent service in crash loop"
    def match(self, ctx):
        crashes = [e for e in ctx._events if e.source == "service_state" and e.severity in ("error", "critical")]
        if len(crashes) >= 3:
            names = list({e.fields.get("service", "?") for e in crashes})
            return self._suggestion(f"Service crash loop detected: {', '.join(names)}", 0.9, {"services": names})
        return None

class ServiceDegraded(Rule):
    id = "service.degraded"
    summary = "Agent service degraded"
    def match(self, ctx):
        degraded = [e for e in ctx._events if e.source == "service_state" and e.severity == "warning"]
        if degraded:
            return self._suggestion("One or more services degraded — check System tab", 0.6, {"count": len(degraded)})
        return None
