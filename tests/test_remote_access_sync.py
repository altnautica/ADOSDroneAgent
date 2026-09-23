"""The remote-access tunnel unit follows the config that says whether it runs.

The regression: turning the Cloudflare tunnel off (or the provider to none)
saved the config and changed nothing else, so ``cloudflared`` kept the node
publicly reachable while every surface said the tunnel was inactive.
"""

from __future__ import annotations

from pathlib import Path

import ados.core.config.writer as writer
import ados.core.remote_access_sync as remote_access_sync
from ados.core.config.writer import set_config_values
from ados.core.remote_access_sync import sync_after_config_write


def _remote(provider: str, enabled: bool, **cf) -> dict:
    return {"remote_access": {"provider": provider, "cloudflare": {"enabled": enabled, **cf}}}


class _Recorder:
    def __init__(self) -> None:
        self.calls: list[list[str]] = []

    def __call__(self, argv, **_kwargs):
        self.calls.append(list(argv))

        class _Done:
            returncode = 0
            stderr = b""

        return _Done()


def test_turning_the_tunnel_off_stops_and_disables_the_unit(monkeypatch) -> None:
    rec = _Recorder()
    monkeypatch.setattr(remote_access_sync.subprocess, "run", rec)

    sync_after_config_write(_remote("cloudflare", True), _remote("cloudflare", False))
    assert rec.calls == [["systemctl", "--no-block", "disable", "--now", "cloudflared"]]

    rec.calls.clear()
    sync_after_config_write(_remote("cloudflare", True), _remote("none", True))
    assert rec.calls == [["systemctl", "--no-block", "disable", "--now", "cloudflared"]]


def test_turning_the_tunnel_on_starts_and_enables_the_unit(monkeypatch) -> None:
    rec = _Recorder()
    monkeypatch.setattr(remote_access_sync.subprocess, "run", rec)

    # From a document with no remote_access block at all (the packaged default).
    sync_after_config_write({"agent": {"name": "x"}}, _remote("cloudflare", True))
    assert rec.calls == [["systemctl", "--no-block", "enable", "--now", "cloudflared"]]


def test_unrelated_writes_never_touch_the_tunnel(monkeypatch) -> None:
    rec = _Recorder()
    monkeypatch.setattr(remote_access_sync.subprocess, "run", rec)

    on = _remote("cloudflare", True)
    sync_after_config_write(on, {**on, "agent": {"name": "renamed"}})
    # Enabled flag alone, provider none: still off, nothing to do.
    sync_after_config_write({}, _remote("none", True))
    sync_after_config_write(None, None)
    assert rec.calls == []


def test_a_renamed_unit_moves_the_tunnel(monkeypatch) -> None:
    rec = _Recorder()
    monkeypatch.setattr(remote_access_sync.subprocess, "run", rec)

    sync_after_config_write(
        _remote("cloudflare", True),
        _remote("cloudflare", True, service_name="cloudflared-node"),
    )
    assert rec.calls == [
        ["systemctl", "--no-block", "disable", "--now", "cloudflared"],
        ["systemctl", "--no-block", "enable", "--now", "cloudflared-node"],
    ]


def test_a_non_tunnel_unit_name_is_never_driven(monkeypatch) -> None:
    rec = _Recorder()
    monkeypatch.setattr(remote_access_sync.subprocess, "run", rec)

    sync_after_config_write(
        _remote("cloudflare", True, service_name="ados-control"),
        _remote("cloudflare", False, service_name="ados-control"),
    )
    assert rec.calls == []


def test_a_config_write_that_turns_the_tunnel_off_stops_it(monkeypatch, tmp_path: Path) -> None:
    """The whole path: a settings write through the one config writer."""
    config = tmp_path / "config.yaml"
    config.write_text(
        "remote_access:\n  provider: cloudflare\n  cloudflare:\n    enabled: true\n",
        encoding="utf-8",
    )
    monkeypatch.setattr(writer, "CONFIG_YAML", config)
    tunnel_calls: list[list[str]] = []

    def _run(argv, **_kwargs):
        if "cloudflared" in argv:
            tunnel_calls.append(list(argv))

        class _Done:
            returncode = 0
            stderr = b""

        return _Done()

    monkeypatch.setattr(remote_access_sync.subprocess, "run", _run)

    result = set_config_values({"remote_access.cloudflare.enabled": False}, path=config)
    assert result.ok and result.wrote
    assert tunnel_calls == [["systemctl", "--no-block", "disable", "--now", "cloudflared"]]
