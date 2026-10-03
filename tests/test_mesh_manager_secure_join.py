"""The mesh joins only with authenticated keying, never open.

802.11s comes up through wpa_supplicant mesh mode with SAE keyed from the
deployment PSK, IBSS through IBSS-RSN. When the secure join cannot be brought
up the node stays off the mesh: there is no fallback to an open
``iw ... mesh join`` / ``ibss join`` that any radio knowing the mesh_id could
ride onto the batman-adv fabric.
"""

from __future__ import annotations

import stat
from pathlib import Path

import pytest

from ados.services.ground_station import mesh_manager as mm

_PSK = bytes(range(32))


def test_80211s_config_uses_sae_keyed_from_the_psk() -> None:
    passphrase = mm._mesh_passphrase(_PSK)
    conf = mm._wpa_supplicant_conf("802.11s", "ados-abc", passphrase, 2412)
    lines = {line.strip() for line in conf.splitlines()}
    assert 'ssid="ados-abc"' in lines
    assert "mode=5" in lines
    assert "frequency=2412" in lines
    assert "key_mgmt=SAE" in lines
    assert f'sae_password="{passphrase}"' in lines
    assert "ieee80211w=2" in lines


def test_ibss_config_uses_rsn_ccmp_keyed_from_the_psk() -> None:
    passphrase = mm._mesh_passphrase(_PSK)
    conf = mm._wpa_supplicant_conf("ibss", "ados-abc", passphrase, 2437)
    lines = {line.strip() for line in conf.splitlines()}
    for expected in (
        "mode=1",
        "frequency=2437",
        "proto=RSN",
        "key_mgmt=WPA-PSK",
        "pairwise=CCMP",
        "group=CCMP",
        f'psk="{passphrase}"',
    ):
        assert expected in lines


def test_passphrase_is_a_valid_wpa_passphrase_shared_by_every_holder() -> None:
    phrase = mm._mesh_passphrase(_PSK)
    assert 8 <= len(phrase) <= 63
    assert phrase == mm._mesh_passphrase(bytes(_PSK))
    assert phrase != mm._mesh_passphrase(bytes(reversed(_PSK)))


@pytest.mark.parametrize(
    "mesh_id",
    ['ados"\n}\nnetwork={', "ados\nkey_mgmt=NONE", "", "x" * 33, " ados", "ados\\"],
)
def test_unsafe_mesh_id_is_refused(mesh_id: str) -> None:
    with pytest.raises(ValueError):
        mm._wpa_supplicant_conf("802.11s", mesh_id, mm._mesh_passphrase(_PSK), 2412)


def test_short_passphrase_is_refused_for_ibss() -> None:
    with pytest.raises(ValueError):
        mm._wpa_supplicant_conf("ibss", "ados-abc", "short", 2412)


class _DeadWpa:
    """A wpa_supplicant that exited at once (no SAE support, bad driver)."""

    pid = 4242
    returncode = 1

    def poll(self) -> int:
        return 1


def test_failed_secure_join_refuses_without_an_open_join(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    commands: list[list[str]] = []

    def _fake_run(cmd: list[str], timeout: float = 10.0) -> tuple[int, str, str]:
        commands.append(cmd)
        return 0, "", ""

    spawned: list[list[str]] = []

    def _fake_popen(argv: list[str], **_kwargs: object) -> _DeadWpa:
        spawned.append(argv)
        return _DeadWpa()

    run_dir = tmp_path / "mesh"
    monkeypatch.setattr(mm, "_run", _fake_run)
    monkeypatch.setattr(mm.subprocess, "Popen", _fake_popen)
    monkeypatch.setattr(mm, "_WPA_RUN_DIR", run_dir)

    for carrier in ("802.11s", "ibss"):
        assert mm._bring_up_mesh_iface("wlan1", carrier, "ados-abc", _PSK, 1) is None

    # wpa_supplicant was the only way the join was attempted.
    assert [argv[0] for argv in spawned] == ["wpa_supplicant", "wpa_supplicant"]
    assert not any("join" in cmd for cmd in commands), commands
    # The keyed config was written private to root.
    conf = run_dir / "wpa_supplicant-wlan1.conf"
    assert stat.S_IMODE(conf.stat().st_mode) == 0o600
    assert stat.S_IMODE(run_dir.stat().st_mode) == 0o700
