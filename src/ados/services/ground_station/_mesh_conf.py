"""wpa_supplicant config rendering and pure decisions for the mesh manager."""

from __future__ import annotations

import os
import subprocess
from pathlib import Path

from ados.core.logging import get_logger

log = get_logger("ground_station.mesh_manager")

_WPA_STOP_GRACE_S = 5.0


def _channel_freq_mhz(channel: int) -> int:
    """2.4 GHz channel to centre frequency. Channel 1 is 2412 MHz.

    Raises ``ValueError`` outside 1..13: mapping an unsupported channel to a
    default would join a different channel than the one config and status
    report.
    """
    if not 1 <= channel <= 13:
        raise ValueError(f"mesh channel {channel} is not a 2.4 GHz channel (1-13)")
    return 2407 + channel * 5


def _mesh_passphrase(psk: bytes) -> str:
    """The mesh passphrase every paired node derives from the shared key.

    ``psk.key`` holds raw random bytes, so they are hex-encoded into printable
    ASCII and cut at 63 characters, the WPA passphrase ceiling IBSS-RSN
    enforces. SAE takes the same string as its ``sae_password``. The same key
    bytes on every node give the same passphrase."""
    return psk.hex()[:63]


def _check_conf_value(name: str, value: str, min_len: int, max_len: int) -> None:
    """Refuse a value that cannot sit verbatim inside a quoted wpa_supplicant
    string: printable ASCII only (no newline, no quote, no backslash), no
    edge whitespace, within the length bounds."""
    if not min_len <= len(value) <= max_len:
        raise ValueError(f"{name} must be {min_len}-{max_len} characters")
    if value != value.strip() or any(
        not (" " <= c <= "~") or c in '"\\' for c in value
    ):
        raise ValueError(
            f"{name} must be printable ASCII without quotes, backslashes or "
            "edge whitespace"
        )


def _wpa_supplicant_conf(
    carrier: str, mesh_id: str, passphrase: str, freq_mhz: int
) -> str:
    """wpa_supplicant config for an authenticated mesh join.

    802.11s: mesh mode (``mode=5``) with SAE and management-frame protection
    required. IBSS: ad-hoc (``mode=1``) with IBSS-RSN, WPA2-PSK over CCMP.
    Raises ValueError for an unknown carrier or a mesh_id / passphrase that
    cannot be written safely (1-32 characters for the mesh_id, 8-63 for the
    passphrase)."""
    _check_conf_value("mesh_id", mesh_id, 1, 32)
    _check_conf_value("mesh passphrase", passphrase, 8, 63)
    if carrier == "802.11s":
        lines = [
            "network={",
            f'\tssid="{mesh_id}"',
            "\tmode=5",
            f"\tfrequency={freq_mhz}",
            "\tkey_mgmt=SAE",
            f'\tsae_password="{passphrase}"',
            "\tieee80211w=2",
            "}",
        ]
    elif carrier == "ibss":
        lines = [
            # IBSS-RSN needs the driver-side scan/selection mode.
            "ap_scan=2",
            "network={",
            f'\tssid="{mesh_id}"',
            "\tmode=1",
            f"\tfrequency={freq_mhz}",
            "\tproto=RSN",
            "\tkey_mgmt=WPA-PSK",
            "\tpairwise=CCMP",
            "\tgroup=CCMP",
            f'\tpsk="{passphrase}"',
            "}",
        ]
    else:
        raise ValueError(f"unknown mesh carrier {carrier!r}")
    return "\n".join(lines) + "\n"


def _write_private(path: Path, text: str) -> None:
    """Write ``text`` to ``path`` as a 0600 file in a 0700 directory."""
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="ascii") as f:
        os.fchmod(f.fileno(), 0o600)
        f.write(text)


def _stop_wpa_supplicant(proc: subprocess.Popen[bytes]) -> None:
    if proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=_WPA_STOP_GRACE_S)
    except subprocess.TimeoutExpired:
        proc.kill()
        try:
            proc.wait(timeout=1.0)
        except subprocess.TimeoutExpired:
            log.warning("wpa_supplicant_kill_unreaped", pid=proc.pid)


def _gateway_mode_for(role: str, cloud_uplink: str, has_uplink: bool) -> str:
    """The batman gateway mode for this node.

    ``force_on`` advertises, ``force_off`` does not, ``auto`` advertises iff
    the uplink is live. A receiver that does not advertise runs as a gateway
    client; every other non-advertising node runs with gateway mode off."""
    if cloud_uplink == "force_on":
        advertise = True
    elif cloud_uplink == "force_off":
        advertise = False
    else:  # auto
        advertise = has_uplink
    if advertise:
        return "server"
    return "client" if role == "receiver" else "off"
