"""Shared reader for the stable-MAC pin state.

The pin state is written by the Rust installer step and kept current by the
supervisor reconciler. The native cloud service composes it into the status
heartbeat; the REST network surface projects it too. This module owns the read
so both see one source of truth rather than each parsing the file.

The enrichment builders that used to live here went with the packaged
heartbeat assembler that called them; the native heartbeat composes its own
blocks.
"""

from __future__ import annotations

import json
from typing import Any

from ados.core.paths import MAC_PINS_STATE_PATH


def read_mac_pins_state() -> dict[str, Any] | None:
    """Read the stable-MAC pin state (:data:`MAC_PINS_STATE_PATH`).

    Returns the parsed document, or ``None`` when the file is absent or
    malformed — a node with no pinned adapters is the normal case, not a fault.
    """
    try:
        data = json.loads(MAC_PINS_STATE_PATH.read_text())
    except (OSError, ValueError):
        return None
    return data if isinstance(data, dict) else None
