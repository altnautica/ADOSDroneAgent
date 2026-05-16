"""Small helpers shared across the main-entry sub-modules.

Kept private so the public surface (``main``, ``AgentApp``) is what
appears in the package barrel.
"""

from __future__ import annotations

import socket


def _get_local_ip() -> str:
    """Detect local IP via UDP socket probe (works without mDNS)."""
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect(("8.8.8.8", 80))
        ip = s.getsockname()[0]
        s.close()
        return ip
    except OSError:
        return "127.0.0.1"


__all__ = ["_get_local_ip"]
