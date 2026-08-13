"""View aggregators that compose the live status JSON.

Each helper returns a dict shape consumed by one or more of the
OLED dashboard, the GCS Hardware tab, the public status route, and
the WHEP / mesh sub-routers. The functions read from agent config
and the WFB stats file.
"""

from __future__ import annotations

from typing import Any


def _read_wfb_view(app: Any) -> dict[str, Any]:
    # WfbConfig lives at app.config.video.wfb, not app.config.wfb. The
    # earlier lookup at the root always returned None and the GET view
    # silently returned defaults (channel=0, profile=default, fec=8/12)
    # regardless of what the operator actually configured.
    video_cfg = getattr(app.config, "video", None)
    wfb_cfg = getattr(video_cfg, "wfb", None) if video_cfg is not None else None
    return {
        "channel": getattr(wfb_cfg, "channel", 0) if wfb_cfg is not None else 0,
        "bitrate_profile": getattr(wfb_cfg, "bitrate_profile", "default")
        if wfb_cfg is not None
        else "default",
        "fec": getattr(wfb_cfg, "fec", "8/12") if wfb_cfg is not None else "8/12",
    }



__all__ = [
    "_read_wfb_view",
]
