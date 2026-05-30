"""Radio heartbeat block builder + WFB status fetch helper.

Shapes the forward-compatible `radio` block carried by the cloud heartbeat
and the LAN-direct status payload from a `WfbManager.get_status()` view (or
an HTTP fallback when no in-process manager is available). Kept as a small
library module with no supervisor coupling so any payload builder can import
it directly.
"""

from __future__ import annotations

from typing import Any

# 5 GHz channel → centre-frequency map. Covers the WFB-ng channel set
# the agent advertises today; values outside this map yield None and the
# GCS draws a blank cell.
_CHANNEL_TO_FREQ_MHZ: dict[int, int] = {
    36: 5180,
    48: 5240,
    149: 5745,
    153: 5765,
    157: 5785,
    161: 5805,
    165: 5825,
}


def _channel_to_freq(channel: object) -> int | None:
    """Return the centre frequency in MHz for a known 5 GHz channel."""
    try:
        ch_int = int(channel)
    except (TypeError, ValueError):
        return None
    return _CHANNEL_TO_FREQ_MHZ.get(ch_int)


def _detect_radio_driver_name(interface: str | None) -> str | None:
    """Best-effort kernel driver name for the WFB monitor interface.

    Reads `/sys/class/net/<iface>/device/uevent` for the `DRIVER=` line.
    Returns the short name (e.g. "8812eu") or None if the iface is empty
    or the file is unreadable.
    """
    if not interface:
        return None
    try:
        from pathlib import Path

        path = Path("/sys/class/net") / interface / "device" / "uevent"
        if not path.is_file():
            return None
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
            if line.startswith("DRIVER="):
                return line.split("=", 1)[1].strip() or None
    except OSError:
        return None
    return None


def build_radio_block(wfb_status: dict[str, Any] | None) -> dict[str, Any]:
    """Shape a forward-compatible `radio` heartbeat block.

    `wfb_status` is the dict returned by `WfbManager.get_status()` (or
    None when the manager is absent — drone profile but the wfb service
    crashed, RTL not plugged in, ground-station profile, etc.). The GCS
    keys off the presence of the block, not the values; absent state is
    rendered as a "no radio" badge.
    """
    if not wfb_status:
        return {
            "state": "absent",
            "iface": None,
            "driver": None,
            "channel": None,
            "freq_mhz": None,
            "bandwidth_mhz": None,
            "tx_power_dbm": None,
            "tx_power_max_dbm": None,
            "topology": None,
            "rssi_dbm": None,
            "snr_db": None,
            "noise_dbm": None,
            "bitrate_kbps": None,
            "fec_recovered": None,
            "fec_lost": None,
            "packets_lost": None,
            "loss_percent": None,
            "mcs_index": None,
            "rx_silent_seconds": None,
            "paired": False,
            "paired_with_device_id": None,
            "paired_at": None,
            "public_key_fingerprint": None,
            "auto_pair_enabled": None,
            "tx_video_stalled": None,
            "tx_video_stall_kills": None,
            "tx_video_recvq_bytes": None,
            "acquire_state": None,
            "channel_locked": None,
            "reacquire_kills": None,
            "valid_rx_packets_per_s": None,
            "adapter_chipset": None,
            "adapter_injection_ok": False,
        }

    iface = wfb_status.get("interface") or None
    channel = wfb_status.get("channel") or None
    rssi = wfb_status.get("rssi_dbm")
    # The link-quality monitor seeds RSSI at -100 dBm before the first
    # real sample lands. Treat that sentinel as "no reading yet".
    if rssi == -100.0:
        rssi = None
    bitrate = wfb_status.get("bitrate_kbps") or None

    return {
        "state": wfb_status.get("state"),
        "iface": iface,
        "driver": _detect_radio_driver_name(iface),
        "channel": channel,
        "freq_mhz": _channel_to_freq(channel),
        "bandwidth_mhz": 20,
        "tx_power_dbm": wfb_status.get("tx_power_dbm"),
        "tx_power_max_dbm": wfb_status.get("tx_power_max_dbm"),
        "topology": wfb_status.get("topology"),
        "rssi_dbm": rssi,
        # RX-side link quality. Populated on both sides: the drone's
        # WfbManager.get_status() and the ground station's /api/wfb view
        # carry the same snake_case keys, so no role branch is needed.
        # On a ground station bitrate_kbps is the received video
        # throughput; rx_silent_seconds is the receive-liveness signal
        # (None on the transmit side, which tracks tx_silent_seconds).
        "snr_db": wfb_status.get("snr_db"),
        "noise_dbm": wfb_status.get("noise_dbm"),
        "bitrate_kbps": bitrate,
        "fec_recovered": wfb_status.get("fec_recovered"),
        "fec_lost": wfb_status.get("fec_failed"),
        "packets_lost": wfb_status.get("packets_lost"),
        "loss_percent": wfb_status.get("loss_percent"),
        "mcs_index": wfb_status.get("mcs_index"),
        "rx_silent_seconds": wfb_status.get("rx_silent_seconds"),
        # Pair-state surface. Source: the on-disk WfbConfig fields,
        # echoed back through WfbManager.get_status() at the top of
        # this module. Heartbeat consumers (GCS, LCD, Convex schema)
        # display these directly. Forward-compatible: omitted on
        # older agent versions, the GCS treats absent as `false`.
        "paired": bool(wfb_status.get("paired", False)),
        "paired_with_device_id": wfb_status.get("paired_with_device_id"),
        "paired_at": wfb_status.get("paired_at"),
        "public_key_fingerprint": wfb_status.get("public_key_fingerprint"),
        "auto_pair_enabled": wfb_status.get("auto_pair_enabled"),
        # Per-stream video transmit liveness (operating rule 37).
        # `tx_video_stalled` flips true when the watchdog sees the UDP
        # 5600 ingress backlog pinned while wfb_tx is alive; the kill
        # counter and current backlog let Mission Control surface a video
        # stall remotely. Absent on the receive side and on older agents.
        "tx_video_stalled": wfb_status.get("tx_video_stalled"),
        "tx_video_stall_kills": wfb_status.get("tx_video_stall_kills"),
        "tx_video_recvq_bytes": wfb_status.get("tx_video_recvq_bytes"),
        # Ground-side receive link quality. acquire_state is the channel
        # acquirer's mode (idle / searching / locked / no-peer);
        # reacquire_kills counts destructive wfb_rx restarts from the
        # valid-packet watchdog so Mission Control can flag a thrashing
        # receiver remotely. Absent on the transmit side and older agents.
        "acquire_state": wfb_status.get("acquire_state"),
        "channel_locked": wfb_status.get("channel_locked"),
        "reacquire_kills": wfb_status.get("reacquire_kills"),
        "valid_rx_packets_per_s": wfb_status.get("valid_rx_packets_per_s"),
        # Selected radio adapter identity + injection verdict. chipset is
        # the label of the adapter the selector picked (null until a real
        # RTL radio is verified); injection_ok is false when no
        # injection-capable adapter was found/proven — the loud stranded
        # radio link signal Mission Control renders.
        "adapter_chipset": wfb_status.get("adapter_chipset"),
        "adapter_injection_ok": bool(wfb_status.get("adapter_injection_ok", False)),
    }


def fetch_wfb_status_via_http(
    host: str = "127.0.0.1",
    port: int = 8080,
    *,
    api_key: str | None = None,
) -> dict[str, Any] | None:
    """HTTP fallback when no in-process WfbManager is available.

    Used by subprocess-mode heartbeat senders that can't import the
    running manager directly. Best-effort: any failure returns None and
    the caller emits an `absent` radio block.

    When the agent is paired the auth middleware rejects unauthenticated
    callers with 401. Callers in subprocess context should pass the
    agent's ``pairing.api_key`` so the probe can complete.
    """
    try:
        import httpx

        headers = {"X-ADOS-Key": api_key} if api_key else None
        with httpx.Client(timeout=0.2) as client:
            resp = client.get(
                f"http://{host}:{port}/api/wfb",
                headers=headers,
            )
            if resp.status_code != 200:
                return None
            data = resp.json()
            if not isinstance(data, dict):
                return None
            return data
    except Exception:
        return None
