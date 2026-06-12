"""Profile gate + agent-config save helpers.

The gate ensures only ground-station-profile agents reach the routes;
drone-profile callers get 404 with a stable error code so the GCS can
distinguish "wrong profile" from "endpoint missing".
"""

from __future__ import annotations

from typing import Any

from fastapi import HTTPException

from ados.api.deps import get_agent_app
from ados.core.profile import current_profile_and_role


def _require_ground_profile() -> Any:
    """Gate: return the agent app if the node's RESOLVED profile is a ground
    station, else 404.

    Resolve through ``current_profile_and_role`` (the same source of truth the
    node advertises on the wire) rather than the raw ``config.agent.profile``
    literal. A ground station installed with ``profile: auto`` carries ``"auto"``
    in the config field but resolves to ``"ground-station"`` via
    ``/etc/ados/profile.conf`` — the raw check 404s that node out of its own
    surface while the heartbeat advertises it as a ground station.
    """
    app = get_agent_app()
    if not is_ground_station(app):
        raise HTTPException(
            status_code=404,
            detail={"error": {"code": "E_PROFILE_MISMATCH"}},
        )
    return app


def is_ground_station(app: Any) -> bool:
    """True when the node's RESOLVED profile is a ground station.

    The single source of truth for the WebSocket routes (mesh, mavlink-ws),
    which gate by closing the socket rather than raising, so they cannot use
    the dependency above but must agree with it (and with the wire profile).
    """
    profile, _role = current_profile_and_role(app.config)
    return profile == "ground-station"


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


__all__ = ["_require_ground_profile", "is_ground_station", "_save_config"]
