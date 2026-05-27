"""Heartbeat reporting mixin: per-service status + full heartbeat payload."""

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


class HeartbeatMixin:
    """Status reporting for the Supervisor."""

    def get_services_status(self) -> list[dict]:
        """Get status of all services for cloud heartbeat / API."""
        result = []
        for name, spec in self._services.items():
            result.append({
                "name": name,
                "status": spec.state,
                "category": spec.category,
                "pid": spec.pid,
                "cpuPercent": round(spec.cpu_percent, 1),
                "memoryMb": round(spec.memory_mb, 1),
                "uptimeSeconds": round(spec.uptime_seconds),
            })
        return result

    def _get_radio_block(self) -> dict[str, Any]:
        """Pull a `WfbManager.get_status()` view, falling back to localhost.

        The supervisor itself does not own a WfbManager (the agent
        process does). When unavailable, we ask the agent's REST surface
        on localhost; on any failure we emit an `absent` block so the
        GCS can render a neutral state.
        """
        wfb = getattr(self, "_wfb_manager", None)
        status: dict[str, Any] | None = None
        if wfb is not None:
            try:
                status = wfb.get_status()
            except Exception:
                status = None
        if status is None:
            status = fetch_wfb_status_via_http()
        return build_radio_block(status)

    def get_heartbeat_payload(self) -> dict:
        """Build full heartbeat payload for cloud status push."""
        try:
            import psutil

            vm = psutil.virtual_memory()
            disk = psutil.disk_usage("/")
            cpu_percent = psutil.cpu_percent(interval=0)
            mem_percent = vm.percent
            disk_percent = disk.percent
            temp = None
            temps = psutil.sensors_temperatures()
            for key in ("cpu_thermal", "cpu-thermal", "coretemp"):
                if key in temps and temps[key]:
                    temp = temps[key][0].current
                    break
        except Exception:
            cpu_percent = 0.0
            mem_percent = 0.0
            disk_percent = 0.0
            vm = None
            disk = None
            temp = None

        from ados import __version__
        from ados.hal.detect import detect_board

        board = detect_board()

        # Setup state and profile source. Surfaced so the GCS fleet view
        # can show an "auto-configured" pill on cards whose profile was
        # picked by the boot-time detect rather than the operator. Both
        # fields are optional: the GCS handles a heartbeat that lacks
        # them gracefully.
        setup_state = "configured"
        profile_source: str | None = None
        try:
            cfg = getattr(self, "config", None)
            agent_profile = (
                str(getattr(cfg.agent, "profile", "") or "") if cfg else ""
            )
            explicit = agent_profile in ("drone", "ground_station")
            if explicit:
                profile_source = "user"
            else:
                from pathlib import Path

                from ados.core.paths import PROFILE_CONF

                if Path(str(PROFILE_CONF)).is_file():
                    try:
                        import yaml

                        data = yaml.safe_load(
                            Path(str(PROFILE_CONF)).read_text(encoding="utf-8")
                        )
                        if isinstance(data, dict):
                            src = data.get("source")
                            if src in ("detected", "tiebreaker", "override", "default"):
                                profile_source = src
                    except Exception:
                        profile_source = None
        except Exception:
            setup_state = "configured"
            profile_source = None

        # Sum process-level metrics
        total_cpu = sum(s.cpu_percent for s in self._services.values())
        total_mem = sum(s.memory_mb for s in self._services.values())

        # Video pipeline restart counter, defensively read.
        vp = getattr(self, "_video_pipeline", None)
        try:
            video_restart_attempts = (
                int(vp.restart_attempts()) if vp is not None else 0
            )
        except Exception:
            video_restart_attempts = 0

        # Foxglove bind status, parity with the in-process builder.
        rm = getattr(self, "_ros_manager", None)
        try:
            foxglove_bind_failed = (
                bool(rm.foxglove_bind_failed()) if rm is not None else False
            )
        except Exception:
            foxglove_bind_failed = False

        return {
            "version": __version__,
            "uptimeSeconds": self.uptime_seconds,
            "boardName": board.name if board else "unknown",
            "boardTier": board.tier if board else 0,
            "boardSoc": board.soc if board else "",
            "boardArch": board.arch if board else "",
            "cpuPercent": cpu_percent,
            "memoryPercent": mem_percent,
            "diskPercent": disk_percent,
            "temperature": temp,
            "memoryUsedMb": round(vm.used / (1024 * 1024)) if vm else 0,
            "memoryTotalMb": round(vm.total / (1024 * 1024)) if vm else 0,
            "diskUsedGb": round(disk.used / (1024**3), 1) if disk else 0,
            "diskTotalGb": round(disk.total / (1024**3), 1) if disk else 0,
            "cpuCores": psutil.cpu_count() if "psutil" in dir() else 0,
            "boardRamMb": round(vm.total / (1024 * 1024)) if vm else 0,
            "processCpuPercent": round(total_cpu, 1),
            "processMemoryMb": round(total_mem, 1),
            "cpuHistory": list(self._cpu_history),
            "memoryHistory": list(self._memory_history),
            "services": self.get_services_status(),
            "videoRestartAttempts": video_restart_attempts,
            "foxgloveBindFailed": foxglove_bind_failed,
            "setupState": setup_state,
            "profileSource": profile_source,
            # Forward-compatible radio link block; older GCS ignore it.
            "radio": self._get_radio_block(),
        }
