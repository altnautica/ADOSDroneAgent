"""Plugin author test harness.

The harness builds an in-process :class:`PluginContext` wired to fake
IPC so plugin authors can exercise lifecycle hooks under ``pytest``
without the supervisor, UDS, or subprocess machinery. Tests grant
capabilities explicitly and assert against captured events.

Public surface:

* :class:`PluginTestHarness` — context manager + fixture API.
* :func:`load_fixture` — YAML scenario loader (events to replay).

CLI integration: ``ados plugin test <plugin_dir>`` discovers a
``tests/`` folder, injects ``harness`` as a ``pytest`` fixture, and
runs the suite. Manifest field ``agent.test_fixtures`` maps friendly
names to YAML paths the harness can replay by name.
"""

from __future__ import annotations

from ados.sdk.testing.fixtures import FixtureEvent, load_fixture
from ados.sdk.testing.harness import PluginTestHarness, PublishedEvent

__all__ = [
    "PluginTestHarness",
    "PublishedEvent",
    "FixtureEvent",
    "load_fixture",
]
