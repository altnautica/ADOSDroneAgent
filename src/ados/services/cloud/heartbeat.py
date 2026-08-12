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


def read_mac_pins_state() -> dict | None:
    """Read ``/etc/ados/mac-pins.state``.

    Returns the parsed document, or ``None`` when the file is absent or
    malformed — a node with no pinned adapters is the normal case, not a fault.
    """
    from ados.core.paths import ADOS_ETC_DIR

    try:
        return json.loads((ADOS_ETC_DIR / "mac-pins.state").read_text())
    except (OSError, ValueError):
        return None
