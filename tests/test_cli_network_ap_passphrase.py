# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""Tests for ``ados network ap-passphrase``.

Two things matter here and they pull in opposite directions. The passphrase has
to be reachable, because on a headless unit every other surface that shows it
(the OLED, the on-box console, the installer's completion card) needs a screen
the operator does not have. And it has to stay out of a support bundle, because
that archive is the thing an operator emails to a stranger.
"""

from __future__ import annotations

import json
from pathlib import Path

from click.testing import CliRunner

import ados.cli.network as netmod
import ados.services.ground_station.hostapd_manager as hostapd
from ados.cli.network import network_group

_FIXTURE = "KM7QRT4XPN29"


def _install(monkeypatch, tmp_path, body: str | None):
    """Point both the reader and the CLI's message at a temp passphrase file."""
    path = tmp_path / "ap-passphrase"
    if body is not None:
        path.write_text(body, encoding="utf-8")
    monkeypatch.setattr(hostapd, "_PASSPHRASE_PATH", path)
    monkeypatch.setattr(netmod, "AP_PASSPHRASE_PATH", path)
    return path


def test_it_prints_the_passphrase_and_nothing_else(monkeypatch, tmp_path) -> None:
    # Bare stdout is the contract: the output is meant to be piped, so a label
    # or a banner would end up inside whatever the operator pasted it into.
    _install(monkeypatch, tmp_path, f"{_FIXTURE}\n")
    result = CliRunner().invoke(network_group, ["ap-passphrase"])
    assert result.exit_code == 0, result.output
    assert result.output == f"{_FIXTURE}\n"


def test_a_missing_file_exits_non_zero_and_says_why(monkeypatch, tmp_path) -> None:
    # A script that captured an error string as if it were the passphrase would
    # then configure a client with it, so the exit status has to carry the
    # failure rather than the text.
    path = _install(monkeypatch, tmp_path, None)
    result = CliRunner().invoke(network_group, ["ap-passphrase"])
    assert result.exit_code != 0
    assert str(path) in result.output
    assert "does not exist" in result.output


def test_an_unreadable_file_names_the_permission_instead(monkeypatch, tmp_path) -> None:
    # 0600 root-owned is the shipped mode, so "present but not readable" is what
    # a non-root operator actually hits, and it needs a different remedy from
    # "this unit has no passphrase".
    _install(monkeypatch, tmp_path, "")
    result = CliRunner().invoke(network_group, ["ap-passphrase"])
    assert result.exit_code != 0
    assert "0600" in result.output
    assert "does not exist" not in result.output


def test_reading_it_does_not_create_or_rotate_the_file(monkeypatch, tmp_path) -> None:
    # The read path must never generate. A status surface that minted one left
    # the operator holding a passphrase hostapd had never loaded.
    path = _install(monkeypatch, tmp_path, None)
    CliRunner().invoke(network_group, ["ap-passphrase"])
    assert not path.exists(), "asking for the passphrase must not mint one"

    path.write_text(f"{_FIXTURE}\n", encoding="utf-8")
    for _ in range(3):
        result = CliRunner().invoke(network_group, ["ap-passphrase"])
        assert result.output == f"{_FIXTURE}\n"
    assert path.read_text(encoding="utf-8") == f"{_FIXTURE}\n"


def test_the_path_is_resolved_not_hardcoded() -> None:
    """The command must reach the passphrase through ``ados.core.paths``.

    A sibling command shipped the Linux literal inline and printed
    ``/etc/ados/...`` on a host whose agent directory is somewhere else
    entirely, which sent the operator looking for a file that could not be
    there.
    """
    from ados.core.paths import AP_PASSPHRASE_PATH

    assert netmod.AP_PASSPHRASE_PATH == AP_PASSPHRASE_PATH
    source = Path(netmod.__file__).read_text(encoding="utf-8")
    assert '"/etc/ados/ap-passphrase"' not in source
    assert "'/etc/ados/ap-passphrase'" not in source


class TestItStaysOutOfASupportBundle:
    """The command exists now, so the redaction that keeps it out has to hold."""

    def test_the_config_capture_redacts_the_hotspot_password(self) -> None:
        # `GET /api/config` is the collector that would carry it: a configured
        # passphrase lives at network.hotspot.password, and that response goes
        # into the archive as JSON.
        from ados.cli.support import REDACTED, redact

        body = json.dumps({"network": {"hotspot": {"password": _FIXTURE}}}, indent=2)
        out = redact(body)
        assert _FIXTURE not in out
        assert REDACTED in out
        # The key survives, so the reader still learns that one is configured.
        assert "password" in out

    def test_the_hostapd_conf_form_is_covered_too(self) -> None:
        from ados.cli.support import redact

        assert _FIXTURE not in redact(f"wpa_passphrase={_FIXTURE}")
        assert _FIXTURE not in redact(f"passphrase: {_FIXTURE}")

    def test_no_collector_reads_the_passphrase_file(self) -> None:
        # Redaction is key-driven, and this file is a bare value with no key in
        # front of it, so redaction could not save it. The defence is that
        # nothing collects it at all.
        import ados.cli.support as sup

        source = Path(sup.__file__).read_text(encoding="utf-8")
        assert "ap-passphrase" not in source
        assert "AP_PASSPHRASE_PATH" not in source

    def test_a_generated_bundle_carries_none_of_it(self, tmp_path, monkeypatch) -> None:
        # End to end over the real collect(): every written file passes through
        # redaction, so a config response carrying the passphrase lands clean.
        import ados.cli.support as sup

        monkeypatch.setattr(
            sup,
            "_collectors",
            lambda: [
                (
                    "api/config.json",
                    lambda: sup._redacted_json(
                        {"network": {"hotspot": {"password": _FIXTURE}}}
                    ),
                )
            ],
        )
        sup.collect(tmp_path)
        for written in tmp_path.rglob("*"):
            if written.is_file():
                assert _FIXTURE not in written.read_text(encoding="utf-8"), written
