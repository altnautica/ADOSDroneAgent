from .base import Rule, Suggestion
from typing import TYPE_CHECKING
if TYPE_CHECKING:
    from ..correlator import ContextWindow

class FcPreArmWarning(Rule):
    id = "fc.pre_arm_warning"
    summary = "FC pre-arm check failed"
    def match(self, ctx):
        fc = [e for e in ctx._events if e.source == "mavlink" and e.fields.get("type") == "pre_arm"]
        if fc:
            return self._suggestion("FC pre-arm check failed — check System tab", 0.9, {"events": len(fc)})
        return None

class FcFailsafeTrigger(Rule):
    id = "fc.failsafe_trigger"
    summary = "FC failsafe triggered"
    def match(self, ctx):
        fs = [e for e in ctx._events if e.source == "mavlink" and e.fields.get("type") == "failsafe"]
        if fs:
            return self._suggestion("FC failsafe triggered — check battery, RC, and GCS links", 0.95, {"count": len(fs)})
        return None
