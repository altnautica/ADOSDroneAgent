from .base import Rule, Suggestion
from typing import TYPE_CHECKING
if TYPE_CHECKING:
    from ..correlator import ContextWindow

class CameraUsbReset(Rule):
    id = "camera.usb_reset"
    summary = "Camera USB reset detected"
    def match(self, ctx):
        resets = [e for e in ctx._events if e.source == "dmesg" and "usb" in e.fields.get("message", "").lower() and "reset" in e.fields.get("message", "").lower()]
        video_errors = [e for e in ctx._events if e.source == "video" and e.severity == "error"]
        if resets and video_errors:
            return self._suggestion("USB reset correlates with video pipeline error — restart ados-video", 0.85, {"resets": len(resets), "video_errors": len(video_errors)}, ["services.restart"])
        return None
