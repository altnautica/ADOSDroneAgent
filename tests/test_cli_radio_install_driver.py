"""Tests for `ados radio install-driver`."""

from __future__ import annotations

from unittest.mock import MagicMock, patch

from click.testing import CliRunner

from ados.cli.radio import radio_group

_SCRIPT = "/opt/ados/source/scripts/drivers/install-rtl8812eu.sh"


def _run(args=("install-driver",)):
    return CliRunner().invoke(radio_group, list(args))


def test_install_driver_skips_when_module_present():
    with (
        patch("ados.cli.radio.platform.system", return_value="Linux"),
        patch("ados.cli.radio._rtl_module_present", return_value=True),
        patch("ados.cli.radio.subprocess.run") as run,
    ):
        result = _run()
    assert result.exit_code == 0
    assert "already installed" in result.output
    run.assert_not_called()


def test_install_driver_advises_update_when_no_source():
    with (
        patch("ados.cli.radio.platform.system", return_value="Linux"),
        patch("ados.cli.radio._rtl_module_present", return_value=False),
        patch("ados.cli.radio._resolve_driver_script", return_value=None),
    ):
        result = _run()
    assert result.exit_code == 1
    assert "ados update" in result.output


def test_install_driver_runs_script_as_root():
    completed = MagicMock(returncode=0)
    with (
        patch("ados.cli.radio.platform.system", return_value="Linux"),
        patch("ados.cli.radio._rtl_module_present", return_value=False),
        patch("ados.cli.radio._resolve_driver_script", return_value=_SCRIPT),
        patch("ados.cli.radio.os.geteuid", return_value=0),
        patch("ados.cli.radio.subprocess.run", return_value=completed) as run,
    ):
        result = _run()
    assert result.exit_code == 0
    assert "installed" in result.output
    argv = run.call_args[0][0]
    assert argv[0] == "bash"
    assert argv[1].endswith("install-rtl8812eu.sh")


def test_install_driver_elevates_with_sudo_when_not_root():
    completed = MagicMock(returncode=0)
    with (
        patch("ados.cli.radio.platform.system", return_value="Linux"),
        patch("ados.cli.radio._rtl_module_present", return_value=False),
        patch("ados.cli.radio._resolve_driver_script", return_value=_SCRIPT),
        patch("ados.cli.radio.os.geteuid", return_value=1000),
        patch("ados.cli.radio.shutil.which", return_value="/usr/bin/sudo"),
        patch("ados.cli.radio.subprocess.run", return_value=completed) as run,
    ):
        result = _run()
    assert result.exit_code == 0
    assert run.call_args[0][0][0] == "sudo"


def test_install_driver_rejects_non_linux():
    with patch("ados.cli.radio.platform.system", return_value="Darwin"):
        result = _run()
    assert result.exit_code == 1
    assert "Linux" in result.output
