"""Tests for the ground-station modem-status endpoint."""

from __future__ import annotations

import asyncio
from typing import Any

import pytest

from ados.api.routes.ground_station import modem as modem_module

# ── Helpers to mock asyncio.create_subprocess_exec ──────────────────


class _FakeProcess:
    def __init__(self, stdout: bytes, returncode: int = 0) -> None:
        self._stdout = stdout
        self.returncode = returncode

    async def communicate(self) -> tuple[bytes, bytes]:
        return self._stdout, b""

    def kill(self) -> None:  # pragma: no cover - never timed out in tests
        pass


def _patch_run(monkeypatch: pytest.MonkeyPatch, table: dict[str, _FakeProcess]) -> None:
    """Patch ``asyncio.create_subprocess_exec`` to dispatch by argv prefix.

    Keys in ``table`` are short argv signatures we match against the
    flattened command, e.g. ``"which mmcli"`` or ``"mmcli -L"``.
    """

    async def _create(*args: Any, **_: Any) -> _FakeProcess:
        cmd = " ".join(str(a) for a in args)
        for prefix, proc in table.items():
            if cmd.startswith(prefix):
                return proc
        return _FakeProcess(b"", returncode=127)

    monkeypatch.setattr(asyncio, "create_subprocess_exec", _create)


def _reset_cache() -> None:
    modem_module._cache_value = None
    modem_module._cache_ts = 0.0


@pytest.mark.asyncio
async def test_modem_status_no_mmcli_returns_present_false(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _reset_cache()
    _patch_run(
        monkeypatch,
        {"which mmcli": _FakeProcess(b"", returncode=1)},
    )
    snap = await modem_module._build_snapshot()
    assert snap == {
        "present": False,
        "reason": "modemmanager_not_installed",
    }


@pytest.mark.asyncio
async def test_modem_status_no_modem_returns_no_modem(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _reset_cache()
    _patch_run(
        monkeypatch,
        {
            "which mmcli": _FakeProcess(b"/usr/bin/mmcli\n"),
            "mmcli -L": _FakeProcess(b"No modems were found\n", returncode=0),
        },
    )
    snap = await modem_module._build_snapshot()
    assert snap == {"present": False, "reason": "no_modem"}


@pytest.mark.asyncio
async def test_modem_status_present_with_signal(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _reset_cache()
    list_out = (
        b"    /org/freedesktop/ModemManager1/Modem/0  [QUECTEL] EM7565\n"
    )
    modem_out = (
        b"modem.generic.signal-quality.value : 78\n"
        b"modem.generic.access-technologies.value : lte\n"
        b"modem.generic.bearers.value : /org/freedesktop/ModemManager1/Bearer/3\n"
        b"modem.generic.bands.value : eutran-40\n"
        b"modem.3gpp.operator-name : Carrier\n"
    )
    signal_out = (
        b"modem.signal.lte.rsrp : -92\n"
        b"modem.signal.lte.rsrq : -10\n"
        b"modem.signal.lte.snr  : 12\n"
        b"modem.signal.lte.rssi : -68\n"
    )
    bearer_out = (
        b"bearer.ipv4-config.address : 10.1.2.3\n"
    )
    _patch_run(
        monkeypatch,
        {
            "which mmcli": _FakeProcess(b"/usr/bin/mmcli\n"),
            "mmcli -L": _FakeProcess(list_out),
            "mmcli -m 0 --signal-get": _FakeProcess(signal_out),
            "mmcli -m 0 -K": _FakeProcess(modem_out),
            "mmcli -b 3 -K": _FakeProcess(bearer_out),
        },
    )
    snap = await modem_module._build_snapshot()
    assert snap["present"] is True
    assert snap["tech"] == "lte"
    assert snap["band"] == "eutran-40"
    assert snap["operator"] == "Carrier"
    assert snap["rsrp_dbm"] == -92
    assert snap["rsrq_db"] == -10
    assert snap["sinr_db"] == 12
    assert snap["rssi_dbm"] == -68
    assert snap["ip"] == "10.1.2.3"


@pytest.mark.asyncio
async def test_modem_status_present_without_bearer(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _reset_cache()
    list_out = b"    /org/freedesktop/ModemManager1/Modem/0  [Quectel] EG25\n"
    modem_out = (
        b"modem.generic.signal-quality.value : 50\n"
        b"modem.generic.access-technologies.value : lte\n"
        b"modem.generic.bearers.value : --\n"
    )
    _patch_run(
        monkeypatch,
        {
            "which mmcli": _FakeProcess(b"/usr/bin/mmcli\n"),
            "mmcli -L": _FakeProcess(list_out),
            "mmcli -m 0 --signal-get": _FakeProcess(b""),
            "mmcli -m 0 -K": _FakeProcess(modem_out),
        },
    )
    snap = await modem_module._build_snapshot()
    assert snap["present"] is True
    assert snap["ip"] == ""
    # Signal probes returned nothing — present but values None.
    assert snap["rsrp_dbm"] is None
    assert snap["rsrq_db"] is None


@pytest.mark.asyncio
async def test_modem_status_caches_for_5_seconds(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _reset_cache()
    calls = {"count": 0}

    async def _create(*args: Any, **_: Any) -> _FakeProcess:
        calls["count"] += 1
        cmd = " ".join(str(a) for a in args)
        if cmd.startswith("which mmcli"):
            return _FakeProcess(b"/usr/bin/mmcli\n")
        if cmd.startswith("mmcli -L"):
            return _FakeProcess(b"No modems were found\n")
        return _FakeProcess(b"", returncode=127)

    monkeypatch.setattr(asyncio, "create_subprocess_exec", _create)
    snap1 = await modem_module._cached_snapshot()
    snap2 = await modem_module._cached_snapshot()
    assert snap1 == snap2
    # First call did 2 subprocess spawns (which + mmcli -L); second
    # call should hit the cache and add zero more.
    assert calls["count"] == 2
