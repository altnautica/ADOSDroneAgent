"""Tests for CLI commands: diag and logs."""

from __future__ import annotations

from unittest.mock import MagicMock, patch

from click.testing import CliRunner

from ados.cli.main import cli

runner = CliRunner()


# ─── diag command ────────────────────────────────────────────────────────────


def _mock_virtual_memory():
    m = MagicMock()
    m.total = 8 * 1024 ** 3  # 8 GB
    m.used = 4 * 1024 ** 3
    m.available = 4 * 1024 ** 3
    m.percent = 50.0
    return m


def _mock_cpu_freq():
    m = MagicMock()
    m.current = 2400.0
    return m


def _mock_net_if_addrs():
    import socket
    addr = MagicMock()
    addr.family = socket.AF_INET
    addr.address = "192.168.1.100"
    return {"eth0": [addr]}


def _mock_disk_usage(path):
    m = MagicMock()
    m.total = 100 * 1024 ** 3
    m.used = 40 * 1024 ** 3
    m.free = 60 * 1024 ** 3
    return m


def _mock_detect_board():
    from ados.hal.detect import BoardInfo
    return BoardInfo(
        name="test-board",
        model="Test Model SBC",
        tier=3,
        ram_mb=8192,
        cpu_cores=4,
    )


@patch("ados.cli.main.psutil.virtual_memory", return_value=_mock_virtual_memory())
@patch("ados.cli.main.psutil.cpu_count", return_value=4)
@patch("ados.cli.main.psutil.cpu_freq", return_value=_mock_cpu_freq())
@patch("ados.cli.main.psutil.net_if_addrs", return_value=_mock_net_if_addrs())
@patch("ados.cli.main.psutil.boot_time", return_value=0.0)
@patch("ados.cli.main.shutil.disk_usage", side_effect=_mock_disk_usage)
@patch("ados.cli.main.os.getloadavg", return_value=(1.0, 0.8, 0.5))
@patch("ados.hal.detect.detect_board", return_value=_mock_detect_board())
def test_diag_contains_board_section(
    mock_board, mock_load, mock_disk, mock_boot, mock_net,
    mock_freq, mock_cpu, mock_mem,
):
    """diag output should contain a Board section with expected fields."""
    result = runner.invoke(cli, ["diag"])
    assert result.exit_code == 0
    assert "Board" in result.output
    assert "test-board" in result.output
    assert "Tier:" in result.output


@patch("ados.cli.main.psutil.virtual_memory", return_value=_mock_virtual_memory())
@patch("ados.cli.main.psutil.cpu_count", return_value=4)
@patch("ados.cli.main.psutil.cpu_freq", return_value=_mock_cpu_freq())
@patch("ados.cli.main.psutil.net_if_addrs", return_value=_mock_net_if_addrs())
@patch("ados.cli.main.psutil.boot_time", return_value=0.0)
@patch("ados.cli.main.shutil.disk_usage", side_effect=_mock_disk_usage)
@patch("ados.cli.main.os.getloadavg", return_value=(1.0, 0.8, 0.5))
@patch("ados.hal.detect.detect_board", return_value=_mock_detect_board())
def test_diag_contains_system_section(
    mock_board, mock_load, mock_disk, mock_boot, mock_net,
    mock_freq, mock_cpu, mock_mem,
):
    """diag output should contain System section with OS and Python info."""
    result = runner.invoke(cli, ["diag"])
    assert result.exit_code == 0
    assert "System" in result.output
    assert "Python:" in result.output
    assert "Kernel:" in result.output
    assert "Uptime:" in result.output


@patch("ados.cli.main.psutil.virtual_memory", return_value=_mock_virtual_memory())
@patch("ados.cli.main.psutil.cpu_count", return_value=4)
@patch("ados.cli.main.psutil.cpu_freq", return_value=_mock_cpu_freq())
@patch("ados.cli.main.psutil.net_if_addrs", return_value=_mock_net_if_addrs())
@patch("ados.cli.main.psutil.boot_time", return_value=0.0)
@patch("ados.cli.main.shutil.disk_usage", side_effect=_mock_disk_usage)
@patch("ados.cli.main.os.getloadavg", return_value=(1.0, 0.8, 0.5))
@patch("ados.hal.detect.detect_board", return_value=_mock_detect_board())
def test_diag_contains_network_section(
    mock_board, mock_load, mock_disk, mock_boot, mock_net,
    mock_freq, mock_cpu, mock_mem,
):
    """diag output should show network info."""
    result = runner.invoke(cli, ["diag"])
    assert result.exit_code == 0
    assert "Network" in result.output
    assert "Hostname:" in result.output
    assert "IP:" in result.output


@patch("ados.cli.main.psutil.virtual_memory", return_value=_mock_virtual_memory())
@patch("ados.cli.main.psutil.cpu_count", return_value=4)
@patch("ados.cli.main.psutil.cpu_freq", return_value=_mock_cpu_freq())
@patch("ados.cli.main.psutil.net_if_addrs", return_value=_mock_net_if_addrs())
@patch("ados.cli.main.psutil.boot_time", return_value=0.0)
@patch("ados.cli.main.shutil.disk_usage", side_effect=_mock_disk_usage)
@patch("ados.cli.main.os.getloadavg", return_value=(1.0, 0.8, 0.5))
@patch("ados.hal.detect.detect_board", return_value=_mock_detect_board())
def test_diag_contains_memory_cpu_disk(
    mock_board, mock_load, mock_disk, mock_boot, mock_net,
    mock_freq, mock_cpu, mock_mem,
):
    """diag output should show Memory, CPU, and Disk sections."""
    result = runner.invoke(cli, ["diag"])
    assert result.exit_code == 0
    assert "Memory" in result.output
    assert "8192 MB" in result.output
    assert "CPU" in result.output
    assert "Cores:" in result.output
    assert "Load avg:" in result.output
    assert "Disk" in result.output


@patch("ados.cli.main.psutil.virtual_memory", return_value=_mock_virtual_memory())
@patch("ados.cli.main.psutil.cpu_count", return_value=4)
@patch("ados.cli.main.psutil.cpu_freq", return_value=_mock_cpu_freq())
@patch("ados.cli.main.psutil.net_if_addrs", return_value=_mock_net_if_addrs())
@patch("ados.cli.main.psutil.boot_time", return_value=0.0)
@patch("ados.cli.main.shutil.disk_usage", side_effect=_mock_disk_usage)
@patch("ados.cli.main.os.getloadavg", return_value=(1.0, 0.8, 0.5))
@patch("ados.hal.detect.detect_board", return_value=_mock_detect_board())
def test_diag_contains_agent_and_deps(
    mock_board, mock_load, mock_disk, mock_boot, mock_net,
    mock_freq, mock_cpu, mock_mem,
):
    """diag output should contain Agent and Dependencies sections."""
    result = runner.invoke(cli, ["diag"])
    assert result.exit_code == 0
    assert "Agent" in result.output
    assert "Version:" in result.output
    assert "Dependencies" in result.output
    assert "pymavlink" in result.output
    assert "psutil" in result.output


@patch("ados.cli.main.psutil.virtual_memory", return_value=_mock_virtual_memory())
@patch("ados.cli.main.psutil.cpu_count", return_value=4)
@patch("ados.cli.main.psutil.cpu_freq", return_value=_mock_cpu_freq())
@patch("ados.cli.main.psutil.net_if_addrs", return_value=_mock_net_if_addrs())
@patch("ados.cli.main.psutil.boot_time", return_value=0.0)
@patch("ados.cli.main.shutil.disk_usage", side_effect=_mock_disk_usage)
@patch("ados.cli.main.os.getloadavg", return_value=(1.0, 0.8, 0.5))
@patch("ados.hal.detect.detect_board", return_value=_mock_detect_board())
def test_diag_contains_services_and_fc(
    mock_board, mock_load, mock_disk, mock_boot, mock_net,
    mock_freq, mock_cpu, mock_mem,
):
    """diag output should contain Services and Flight Controller sections."""
    result = runner.invoke(cli, ["diag"])
    assert result.exit_code == 0
    assert "Services" in result.output
    assert "ados-agent" in result.output
    assert "Flight Controller" in result.output
    assert "Connected:" in result.output


@patch("ados.cli.main.psutil.virtual_memory", return_value=_mock_virtual_memory())
@patch("ados.cli.main.psutil.cpu_count", return_value=4)
@patch("ados.cli.main.psutil.cpu_freq", return_value=_mock_cpu_freq())
@patch("ados.cli.main.psutil.net_if_addrs", return_value=_mock_net_if_addrs())
@patch("ados.cli.main.psutil.boot_time", return_value=0.0)
@patch("ados.cli.main.shutil.disk_usage", side_effect=_mock_disk_usage)
@patch("ados.cli.main.os.getloadavg", return_value=(1.0, 0.8, 0.5))
@patch("ados.hal.detect.detect_board", return_value=_mock_detect_board())
def test_diag_contains_temperature(
    mock_board, mock_load, mock_disk, mock_boot, mock_net,
    mock_freq, mock_cpu, mock_mem,
):
    """diag output should have a Temperature section."""
    result = runner.invoke(cli, ["diag"])
    assert result.exit_code == 0
    assert "Temperature" in result.output
    assert "CPU:" in result.output


# ─── logs command ────────────────────────────────────────────────────────────


def test_logs_macos_no_logfile():
    """On macOS with no log file, logs should print a helpful message."""
    with patch("ados.cli.main.platform.system", return_value="Darwin"):
        with patch("pathlib.Path.exists", return_value=False):
            result = runner.invoke(cli, ["logs"])
            assert result.exit_code == 0
            assert "systemd is not available on macOS" in result.output


def test_logs_macos_with_logfile(tmp_path):
    """On macOS with a log file present, logs should read from it."""
    log_file = tmp_path / "agent.log"
    log_file.write_text("line1\nline2\nline3\nline4\nline5\n")

    with patch("ados.cli.main.platform.system", return_value="Darwin"):
        # Patch Path.home to return tmp_path parent so ~/.ados/agent.log works
        ados_dir = tmp_path / ".ados"
        ados_dir.mkdir()
        ados_log = ados_dir / "agent.log"
        ados_log.write_text("log line A\nlog line B\nlog line C\n")

        with patch("pathlib.Path.home", return_value=tmp_path):
            result = runner.invoke(cli, ["logs", "--lines", "2"])
            assert result.exit_code == 0
            assert "log line B" in result.output
            assert "log line C" in result.output


@patch("ados.cli.main.platform.system", return_value="Linux")
@patch("ados.cli.main.subprocess.run")
def test_logs_linux_calls_journalctl(mock_run, mock_sys):
    """On Linux, logs should invoke journalctl."""
    result = runner.invoke(cli, ["logs", "--lines", "20"])
    assert result.exit_code == 0
    mock_run.assert_called_once()
    cmd = mock_run.call_args[0][0]
    assert "journalctl" in cmd
    assert "-u" in cmd
    assert "ados-agent.service" in cmd
    assert "-n" in cmd
    assert "20" in cmd


@patch("ados.cli.main.platform.system", return_value="Linux")
@patch("ados.cli.main.subprocess.run")
def test_logs_linux_follow_flag(mock_run, mock_sys):
    """On Linux, --follow should pass -f to journalctl."""
    result = runner.invoke(cli, ["logs", "--follow"])
    assert result.exit_code == 0
    cmd = mock_run.call_args[0][0]
    assert "-f" in cmd


@patch("ados.cli.main.platform.system", return_value="Linux")
@patch("ados.cli.main.subprocess.run")
def test_logs_linux_since_flag(mock_run, mock_sys):
    """On Linux, --since should be forwarded to journalctl."""
    result = runner.invoke(cli, ["logs", "--since", "1h ago"])
    assert result.exit_code == 0
    cmd = mock_run.call_args[0][0]
    assert "--since" in cmd
    assert "1h ago" in cmd


@patch("ados.cli.main.platform.system", return_value="Linux")
@patch("ados.cli.main.subprocess.run", side_effect=FileNotFoundError)
def test_logs_linux_no_journalctl(mock_run, mock_sys):
    """If journalctl is missing, logs should print an error."""
    result = runner.invoke(cli, ["logs"])
    assert "journalctl not found" in result.output
