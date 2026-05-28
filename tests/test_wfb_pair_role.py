"""Regression tests for the WFB pair-route role resolution.

Earlier the pair routes at ``src/ados/api/routes/wfb.py`` read
``config.agent.profile`` directly and passed the raw value into the
``_agent_role_from_profile`` mapper. When the operator left the config
field on the documented onboarding default (``"auto"``), the mapper
treated anything not literally equal to ``"drone"`` as ``"gs"``, so a
fresh-install drone (where ``/etc/ados/profile.conf`` correctly said
``"drone"`` but the YAML config field said ``"auto"``) was bound as a
ground station and the WFB rendezvous never converged.

The fix routes the lookup through
``ados.core.profile.current_profile_and_role`` which already implements
the documented resolution order:

  1. explicit ``config.agent.profile`` (``"drone"`` / ``"ground_station"``)
  2. ``/etc/ados/profile.conf`` (the runtime probe result) when (1) is
     ``"auto"`` or empty
  3. ``"drone"`` as a final fallback

These tests pin the new behaviour of the ``_current_role`` helper that
the pair routes (``GET /wfb/pair``, ``POST /wfb/pair/local-bind``,
``POST /wfb/pair/unpair``, ``PUT /wfb/pair/auto-pair``) now consult.
"""

from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace

import pytest

from ados.api.routes.wfb import _agent_role_from_profile, _current_role

# --- helpers -----------------------------------------------------------------


def _make_app(profile: str | None) -> SimpleNamespace:
    """Build a minimal stand-in for the API runtime facade.

    The pair routes only ever read ``app.config.agent.profile`` on
    their way into ``_current_role``; this stub mirrors that shape
    without dragging in the full ApiRuntimeTestDouble.
    """
    return SimpleNamespace(config=SimpleNamespace(agent=SimpleNamespace(profile=profile)))


def _write_profile_conf(tmp_path: Path, value: str | None) -> Path:
    """Write a YAML-style /etc/ados/profile.conf at tmp_path.

    Passing ``value=None`` produces no file at all, which models the
    pre-bootstrap state where the runtime probe has not yet run.
    """
    target = tmp_path / "profile.conf"
    if value is not None:
        target.write_text(f"profile: {value}\nsource: detected\n")
    return target


def _patch_profile_conf(monkeypatch: pytest.MonkeyPatch, conf_path: Path) -> None:
    """Point the resolver at our tmp profile.conf instead of /etc/ados/."""
    from ados.core import profile as profile_module

    monkeypatch.setattr(profile_module, "PROFILE_CONF", conf_path)


# --- pure mapper ------------------------------------------------------------


def test_mapper_returns_drone_for_drone_profile() -> None:
    assert _agent_role_from_profile("drone") == "drone"


def test_mapper_returns_gs_for_ground_station_underscore() -> None:
    assert _agent_role_from_profile("ground_station") == "gs"


def test_mapper_returns_gs_for_ground_station_hyphen() -> None:
    """The wire form ``ground-station`` should also map to gs."""
    assert _agent_role_from_profile("ground-station") == "gs"


# --- _current_role: the documented bug fix ----------------------------------


def test_role_drone_when_profile_conf_says_drone_and_config_auto(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """The motivating fix.

    Fresh install: ``config.agent.profile`` defaults to ``"auto"``;
    ``/etc/ados/profile.conf`` carries the runtime-probe result
    (``"drone"`` for a rig with FC + RTL dongle). The resolver must
    return ``"drone"`` so auto-pair runs the server flow.
    """
    _patch_profile_conf(monkeypatch, _write_profile_conf(tmp_path, "drone"))
    assert _current_role(_make_app("auto")) == "drone"


def test_role_gs_when_profile_conf_says_ground_station_and_config_auto(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """Mirror case: same fresh-install flow on a ground-station rig."""
    _patch_profile_conf(
        monkeypatch, _write_profile_conf(tmp_path, "ground_station")
    )
    assert _current_role(_make_app("auto")) == "gs"


def test_role_drone_when_config_explicitly_drone(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """Explicit config value wins regardless of profile.conf state."""
    _patch_profile_conf(monkeypatch, tmp_path / "missing.conf")
    assert _current_role(_make_app("drone")) == "drone"


def test_role_gs_when_config_explicitly_ground_station(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """Explicit ground_station config value wins too."""
    _patch_profile_conf(monkeypatch, tmp_path / "missing.conf")
    assert _current_role(_make_app("ground_station")) == "gs"


def test_role_drone_when_no_profile_conf_and_config_auto(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """No profile.conf, config=auto → safe default is drone.

    This pins the documented final fallback in
    ``current_profile_and_role``: when nothing else resolves, the wire
    profile is ``"drone"``, which maps to the drone bind role. A
    fresh rig with no on-disk hint at least tries the server flow
    rather than silently picking the client flow.
    """
    _patch_profile_conf(monkeypatch, tmp_path / "missing.conf")
    assert _current_role(_make_app("auto")) == "drone"


def test_role_drone_when_config_is_empty_string(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """Empty string is treated the same as ``"auto"`` (fall-through)."""
    _patch_profile_conf(monkeypatch, _write_profile_conf(tmp_path, "drone"))
    assert _current_role(_make_app("")) == "drone"


def test_role_drone_when_config_is_none(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """A None config value also falls through to profile.conf."""
    _patch_profile_conf(monkeypatch, _write_profile_conf(tmp_path, "drone"))
    assert _current_role(_make_app(None)) == "drone"
