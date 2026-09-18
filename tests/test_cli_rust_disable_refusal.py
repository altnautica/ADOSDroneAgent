"""`ados rust disable control` must refuse while the native front owns :8080.

`ados-control.service` execs the native binary when EITHER `control-rust-enabled`
OR `front-rust-enabled` is present, and the installer writes the front marker on
every install with no operator toggle for it. So `disable control` stopped and
disabled the one process serving the LAN port — with the residual FastAPI moved
to an internal Unix socket by the front drop-in, leaving :8080 with no listener —
and printed that it had reverted to a packaged service.
"""

from __future__ import annotations

import pytest
from click.testing import CliRunner

from ados.cli import rust as rust_mod
from ados.cli.rust import rust_group


@pytest.fixture(autouse=True)
def _linux_root(monkeypatch):
    """Pin platform + euid so the command body runs on any dev host."""
    monkeypatch.setattr(rust_mod.platform, "system", lambda: "Linux")
    monkeypatch.setattr(rust_mod.os, "geteuid", lambda: 0)


@pytest.fixture
def etc(tmp_path, monkeypatch):
    """A tmp /etc/ados plus an installed, executable native control binary."""
    binary = tmp_path / "ados-control"
    binary.write_text("#!/bin/sh\n", encoding="utf-8")
    binary.chmod(0o755)
    monkeypatch.setattr(rust_mod, "ADOS_ETC_DIR", tmp_path)
    monkeypatch.setattr(rust_mod, "_CONTROL_BIN", str(binary))
    monkeypatch.setattr(rust_mod, "_binaries_present", lambda svc: True)
    monkeypatch.setattr(rust_mod, "_unit_active", lambda unit: False)
    return tmp_path


def _record_systemctl(monkeypatch) -> list[tuple[str, ...]]:
    """Record every systemctl invocation instead of running one."""
    calls: list[tuple[str, ...]] = []

    def _systemctl(*args, **kwargs):
        calls.append(args)
        return 0

    monkeypatch.setattr(rust_mod, "_systemctl", _systemctl)
    return calls


def test_disable_control_is_refused_while_the_front_owns_the_lan_port(
    etc, monkeypatch
) -> None:
    (etc / "front-rust-enabled").touch()
    (etc / "control-rust-enabled").touch()
    calls = _record_systemctl(monkeypatch)

    result = CliRunner().invoke(rust_group, ["disable", "control"])

    assert result.exit_code != 0, result.output
    assert "refused" in result.output
    assert "reverted to the packaged service" not in result.output
    # Nothing was stopped and the marker is untouched, so the node is still
    # answering on :8080 after the refusal.
    assert calls == []
    assert (etc / "control-rust-enabled").exists()


def test_disable_control_proceeds_when_the_front_is_not_native(
    etc, monkeypatch
) -> None:
    # No front marker: the native surface is only the alternate-port listener,
    # so falling back costs nothing an operator reaches the node by.
    (etc / "control-rust-enabled").touch()
    calls = _record_systemctl(monkeypatch)

    result = CliRunner().invoke(rust_group, ["disable", "control"])

    assert result.exit_code == 0, result.output
    assert "reverted to the packaged service" in result.output
    assert not (etc / "control-rust-enabled").exists()
    assert calls, "the fallback must actually move the units"


def test_a_flip_that_could_not_reach_systemctl_does_not_claim_success(
    etc, monkeypatch
) -> None:
    # No systemctl on PATH: every call reports 127 and no unit moved.
    monkeypatch.setattr(rust_mod.shutil, "which", lambda name: None)

    result = CliRunner().invoke(rust_group, ["enable", "logd"])

    assert result.exit_code != 0, result.output
    assert "native implementation enabled" not in result.output
