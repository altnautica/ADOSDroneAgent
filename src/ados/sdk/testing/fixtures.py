"""YAML scenario loader for :class:`PluginTestHarness`.

A fixture file is a list of event records the harness can replay
into the plugin's subscribers. Schema:

.. code-block:: yaml

    - topic: telemetry.battery
      payload: {voltage_mv: 24800, current_ma: 12100}
      delay_ms: 0    # optional, default 0
    - topic: telemetry.gps
      payload: {fix_type: 3, sat_count: 14}
      delay_ms: 50

The loader is deliberately small — plugin authors who need richer
scenarios can call :meth:`PluginTestHarness.publish_event` directly.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import yaml

from ados.plugins.errors import PluginError


@dataclass(frozen=True)
class FixtureEvent:
    topic: str
    payload: dict[str, Any] = field(default_factory=dict)
    delay_ms: int = 0


def load_fixture(path: str | Path) -> list[FixtureEvent]:
    """Parse a YAML scenario file. Raises :class:`PluginError` on bad input."""
    p = Path(path)
    try:
        text = p.read_text(encoding="utf-8")
    except OSError as exc:
        raise PluginError(f"cannot read fixture {path}: {exc}") from exc
    try:
        data = yaml.safe_load(text)
    except yaml.YAMLError as exc:
        raise PluginError(f"fixture {path} is not valid YAML: {exc}") from exc
    if data is None:
        return []
    if not isinstance(data, list):
        raise PluginError(
            f"fixture {path} top-level must be a list of events, got {type(data).__name__}"
        )
    out: list[FixtureEvent] = []
    for i, raw in enumerate(data):
        if not isinstance(raw, dict):
            raise PluginError(f"fixture {path} entry {i} must be a mapping")
        topic = raw.get("topic")
        if not isinstance(topic, str) or not topic:
            raise PluginError(f"fixture {path} entry {i} missing string 'topic'")
        payload = raw.get("payload", {})
        if not isinstance(payload, dict):
            raise PluginError(
                f"fixture {path} entry {i} 'payload' must be a mapping"
            )
        delay_ms = raw.get("delay_ms", 0)
        if not isinstance(delay_ms, int) or delay_ms < 0:
            raise PluginError(
                f"fixture {path} entry {i} 'delay_ms' must be a non-negative int"
            )
        out.append(FixtureEvent(topic=topic, payload=payload, delay_ms=delay_ms))
    return out
