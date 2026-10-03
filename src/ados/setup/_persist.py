"""The one persist step every setup setter ends with.

A setter validates its whole request first, then names the config leaves it
changes as dotted paths and writes them through the runtime in one locked
write. Nothing is mutated in memory beforehand, so a refused request or a
failed write leaves no half-applied state behind, and the runtime's next
config read reflects exactly what reached disk.
"""

from __future__ import annotations

from collections.abc import Mapping
from typing import Any

from ados.setup.models import SetupActionResult


def persist_config(
    runtime: Any, values: Mapping[str, Any], *, what: str
) -> SetupActionResult | None:
    """Write ``values`` (dotted path → value); ``None`` on success.

    On failure the returned result carries ``ok=False`` and the writer's
    reason, so the caller returns it as-is instead of reporting success.
    """
    if not values:
        return None
    result = runtime.write_config(values)
    if result:
        return None
    reason = getattr(result, "error", None) or "config could not be written to disk"
    return SetupActionResult(ok=False, message=f"{what} not saved: {reason}")


__all__ = ["persist_config"]
