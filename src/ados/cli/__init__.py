"""Shared helpers for the ADOS CLI command groups."""

from __future__ import annotations

import os

# The GCS-facing LAN port is :8080 — the native Rust front, or FastAPI when the
# front is disabled. :8082 is the native control surface's alternate plane, where
# an install that did not finish the native-front cutover (or an operator running
# the control binary alongside FastAPI) leaves it. Trying both means a CLI never
# falsely reports "agent not running" just because it is on the other port.
_LOCAL_CONTROL_PORTS = (8080, 8082)


def api_bases() -> list[str]:
    """Ordered local control-surface base URLs a CLI should try, primary first.

    An explicit ``ADOS_CONTROL_PORT`` overrides the search (the systemd unit pins
    the port per profile).
    """
    env = os.environ.get("ADOS_CONTROL_PORT", "").strip()
    if env.isdigit():
        return [f"http://localhost:{env}"]
    return [f"http://localhost:{port}" for port in _LOCAL_CONTROL_PORTS]


def default_api_base() -> str:
    """The primary local control-surface base URL (the first candidate)."""
    return api_bases()[0]
