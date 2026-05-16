"""Profile gate + agent-config save helpers.

The gate ensures only ground-station-profile agents reach the routes;
drone-profile callers get 404 with a stable error code so the GCS can
distinguish "wrong profile" from "endpoint missing".
"""

from __future__ import annotations

from typing import Any

from fastapi import HTTPException

from ados.api.deps import get_agent_app


def _require_ground_profile() -> Any:
    """Gate: return the agent app if profile is ground_station, else 404."""
    app = get_agent_app()
    profile = getattr(app.config.agent, "profile", "auto")
    if profile != "ground_station":
        raise HTTPException(
            status_code=404,
            detail={"error": {"code": "E_PROFILE_MISMATCH"}},
        )
    return app


def _save_config(app: Any) -> None:
    """Best-effort persist agent config to disk."""
    saver = getattr(app, "save_config", None)
    if callable(saver):
        try:
            saver()
            return
        except Exception:
            pass
    cfg_save = getattr(app.config, "save", None)
    if callable(cfg_save):
        try:
            cfg_save()
        except Exception:
            pass


__all__ = ["_require_ground_profile", "_save_config"]
