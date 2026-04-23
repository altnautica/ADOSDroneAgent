from .base import Rule, Suggestion
from typing import TYPE_CHECKING
if TYPE_CHECKING:
    from ..correlator import ContextWindow

class DiskPressure(Rule):
    id = "storage.disk_pressure"
    summary = "Disk space running low"
    def match(self, ctx):
        sysfs = [e for e in ctx._events if e.source == "sysfs" and e.fields.get("disk_free_pct", 100) < 10]
        if sysfs:
            pct = sysfs[-1].fields.get("disk_free_pct", 0)
            return self._suggestion(f"Disk space at {pct:.0f}% free — World Model eviction recommended", 0.8, {"free_pct": pct})
        return None
