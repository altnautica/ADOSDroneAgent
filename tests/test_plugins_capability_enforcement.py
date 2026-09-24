"""Capability catalog id-set helper tests.

The per-method dispatch gate and the enforced-capability set are NOT covered
here. That gate lives in the native host, and the equivalent coverage is the
host's own capability-enforcement guard, which derives the gated set from the
dispatch table rather than restating it.
"""

from __future__ import annotations

from ados.plugins.capabilities import is_known_agent_capability


def test_is_known_agent_capability() -> None:
    assert is_known_agent_capability("event.publish")
    assert is_known_agent_capability("mavlink.read")
    assert not is_known_agent_capability("not.a.real.cap")
