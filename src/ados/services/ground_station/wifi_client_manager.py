"""WiFi client (station) manager for the ground-station profile (MSN-027).

Phase 3 uplink matrix. The ground station's onboard wlan0 radio can act
as an AP (hostapd_manager) or as a station joining an upstream WiFi
network for internet backhaul. The two modes are mutually exclusive on
the same radio, so this manager coordinates with ados-hostapd via a
file-based advisory lock.

Backend: NetworkManager (`nmcli`). NM persists saved connection
profiles on its own, so we do not re-implement credential storage.
What we do persist is the "client enabled on boot" flag so the uplink
router knows which uplinks to bring up automatically.

Published events (for uplink_router, Wave B):
    WifiClientEvent{kind, ssid, signal, ip, timestamp_ms}

Mutex:
    /var/lock/ados-wlan0.lock via fcntl.flock(LOCK_EX). Held across
    systemctl stop ados-hostapd -> nmcli connect. Released after leave()
    restarts hostapd (if it was the prior owner).

State flag:
    /var/run/ados/ap-was-enabled  -> "1" if hostapd was active when we
    stole wlan0, "0" otherwise. Cleared after restoration.
"""

from __future__ import annotations

import asyncio
import contextlib
import fcntl
import json
import os
import re
import signal
import subprocess
import sys
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import AsyncIterator, Literal

from ados.core.logging import get_logger

log = get_logger("ground_station.wifi_client")

_WLAN_IFACE = "wlan0"
_LOCK_PATH = Path("/var/lock/ados-wlan0.lock")
_AP_FLAG_PATH = Path("/var/run/ados/ap-was-enabled")
_CLIENT_CONFIG_PATH = Path("/etc/ados/ground-station-wifi-client.json")
_HOSTAPD_UNIT = "ados-hostapd.service"


# -------------------- events --------------------


@dataclass(frozen=True)
class WifiClientEvent:
    """Connection-state transition for the WiFi client."""

    kind: Literal["connected", "disconnected", "signal_changed"]
    ssid: str | None
    signal: int | None
    ip: str | None
    timestamp_ms: int


class WifiClientEventBus:
    """Asyncio fanout bus. Mirrors ButtonEventBus semantics."""

    _SENTINEL: object = object()

    def __init__(self, queue_maxsize: int = 32) -> None:
        self._subscribers: list[asyncio.Queue] = []
        self._queue_maxsize = queue_maxsize
        self._closed = False
        self._lock = asyncio.Lock()

    async def publish(self, event: WifiClientEvent) -> None:
        if self._closed:
            return
        async with self._lock:
            targets = list(self._subscribers)
        for q in targets:
            try:
                q.put_nowait(event)
            except asyncio.QueueFull:
                pass

    async def subscribe(self) -> AsyncIterator[WifiClientEvent]:
        queue: asyncio.Queue = asyncio.Queue(maxsize=self._queue_maxsize)
        async with self._lock:
            if self._closed:
                return
            self._subscribers.append(queue)
        try:
            while True:
                item = await queue.get()
                if item is self._SENTINEL:
                    return
                assert isinstance(item, WifiClientEvent)
                yield item
        finally:
            async with self._lock:
                if queue in self._subscribers:
                    self._subscribers.remove(queue)

    async def close(self) -> None:
        async with self._lock:
            self._closed = True
            targets = list(self._subscribers)
        for q in targets:
            with contextlib.suppress(asyncio.QueueFull):
                q.put_nowait(self._SENTINEL)


# -------------------- helpers --------------------


def _now_ms() -> int:
    return int(time.time() * 1000)


async def _run(cmd: list[str], timeout: float = 15.0) -> tuple[int, str, str]:
    """Run a command async. Returns (rc, stdout, stderr)."""
    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        try:
            out, err = await asyncio.wait_for(proc.communicate(), timeout=timeout)
        except asyncio.TimeoutError:
            proc.kill()
            await proc.wait()
            return 124, "", "timeout"
        return (
            proc.returncode or 0,
            out.decode(errors="replace"),
            err.decode(errors="replace"),
        )
    except (OSError, asyncio.CancelledError) as exc:
        log.warning("run_failed", cmd=cmd, error=str(exc))
        return 1, "", str(exc)


def _parse_nmcli_terse(text: str, fields: int) -> list[list[str]]:
    """Parse nmcli -t output. Colon is the separator, backslash escapes."""
    rows: list[list[str]] = []
    for line in text.splitlines():
        if not line.strip():
            continue
        # Split on unescaped colons.
        parts: list[str] = []
        buf: list[str] = []
        i = 0
        while i < len(line):
            ch = line[i]
            if ch == "\\" and i + 1 < len(line):
                buf.append(line[i + 1])
                i += 2
                continue
            if ch == ":":
                parts.append("".join(buf))
                buf = []
                i += 1
                continue
            buf.append(ch)
            i += 1
        parts.append("".join(buf))
        if len(parts) >= fields:
            rows.append(parts[:fields])
    return rows


# -------------------- manager --------------------


class WifiClientManager:
    """NetworkManager-backed WiFi station manager for wlan0."""

    def __init__(self, interface: str = _WLAN_IFACE) -> None:
        self._interface = interface
        self._bus = WifiClientEventBus()
        self._lock_fd: int | None = None
        self._last_status: dict = {}
        self._poll_task: asyncio.Task | None = None

    @property
    def bus(self) -> WifiClientEventBus:
        return self._bus

    # -------------------- lock handling --------------------

    def _acquire_lock(self) -> bool:
        try:
            _LOCK_PATH.parent.mkdir(parents=True, exist_ok=True)
            fd = os.open(str(_LOCK_PATH), os.O_CREAT | os.O_RDWR, 0o644)
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self._lock_fd = fd
            return True
        except (OSError, BlockingIOError) as exc:
            log.warning("wlan0_lock_failed", error=str(exc))
            return False

    def _release_lock(self) -> None:
        if self._lock_fd is not None:
            with contextlib.suppress(OSError):
                fcntl.flock(self._lock_fd, fcntl.LOCK_UN)
                os.close(self._lock_fd)
            self._lock_fd = None

    def _is_hostapd_active(self) -> bool:
        try:
            r = subprocess.run(
                ["systemctl", "is-active", _HOSTAPD_UNIT],
                check=False,
                capture_output=True,
                timeout=5,
            )
            return r.stdout.decode(errors="replace").strip() == "active"
        except (OSError, subprocess.SubprocessError):
            return False

    def _write_ap_flag(self, enabled: bool) -> None:
        try:
            _AP_FLAG_PATH.parent.mkdir(parents=True, exist_ok=True)
            _AP_FLAG_PATH.write_text("1\n" if enabled else "0\n", encoding="utf-8")
        except OSError as exc:
            log.warning("ap_flag_write_failed", error=str(exc))

    def _read_ap_flag(self) -> bool:
        if not _AP_FLAG_PATH.is_file():
            return False
        try:
            return _AP_FLAG_PATH.read_text(encoding="utf-8").strip() == "1"
        except OSError:
            return False

    def _clear_ap_flag(self) -> None:
        with contextlib.suppress(OSError):
            if _AP_FLAG_PATH.is_file():
                _AP_FLAG_PATH.unlink()

    # -------------------- client enabled flag --------------------

    def _load_client_config(self) -> dict:
        if not _CLIENT_CONFIG_PATH.is_file():
            return {"enabled_on_boot": False, "last_ssid": None}
        try:
            return json.loads(_CLIENT_CONFIG_PATH.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return {"enabled_on_boot": False, "last_ssid": None}

    def _save_client_config(self, data: dict) -> None:
        try:
            _CLIENT_CONFIG_PATH.parent.mkdir(parents=True, exist_ok=True)
            tmp = _CLIENT_CONFIG_PATH.with_suffix(".json.tmp")
            tmp.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
            os.rename(tmp, _CLIENT_CONFIG_PATH)
        except OSError as exc:
            log.warning("client_config_write_failed", error=str(exc))

    async def set_enabled_on_boot(self, enabled: bool) -> dict:
        data = self._load_client_config()
        data["enabled_on_boot"] = bool(enabled)
        self._save_client_config(data)
        return data

    # -------------------- public API --------------------

    async def scan(self, timeout_s: float = 10.0) -> list[dict]:
        """Scan for nearby networks. Returns list sorted by signal desc."""
        rc, out, err = await _run(
            [
                "nmcli", "-t",
                "-f", "SSID,BSSID,SIGNAL,SECURITY,IN-USE",
                "device", "wifi", "list", "--rescan", "yes",
            ],
            timeout=timeout_s,
        )
        if rc != 0:
            log.warning("wifi_scan_failed", rc=rc, err=err.strip())
            return []
        rows = _parse_nmcli_terse(out, 5)
        results: list[dict] = []
        for row in rows:
            ssid, bssid, sig, security, in_use = row
            if not ssid:
                continue
            try:
                sig_int = int(sig)
            except ValueError:
                sig_int = 0
            results.append({
                "ssid": ssid,
                "bssid": bssid,
                "signal": sig_int,
                "security": security or "--",
                "in_use": in_use.strip() == "*",
            })
        results.sort(key=lambda r: r["signal"], reverse=True)
        return results

    async def join(
        self,
        ssid: str,
        passphrase: str | None,
        force: bool = False,
    ) -> dict:
        """Join a WiFi network. Coordinates wlan0 with hostapd."""
        if not ssid or not isinstance(ssid, str):
            return {"joined": False, "error": "ssid_required", "ip": None, "gateway": None}

        ap_active = self._is_hostapd_active()
        if ap_active and not force:
            return {
                "joined": False,
                "error": "wlan0_busy_ap_active",
                "hint": "Stop AP first or force",
                "ip": None,
                "gateway": None,
            }

        if not self._acquire_lock():
            return {
                "joined": False,
                "error": "wlan0_locked",
                "ip": None,
                "gateway": None,
            }

        try:
            self._write_ap_flag(ap_active)
            if ap_active:
                log.info("stopping_hostapd_for_client", ssid=ssid)
                await _run(["systemctl", "stop", _HOSTAPD_UNIT], timeout=10)
                await asyncio.sleep(1.0)

            cmd = ["nmcli", "device", "wifi", "connect", ssid]
            if passphrase:
                cmd += ["password", passphrase]
            cmd += ["ifname", self._interface]

            rc, out, err = await _run(cmd, timeout=30)
            if rc != 0:
                log.warning("wifi_join_failed", ssid=ssid, err=err.strip())
                # Restore AP if we stole it.
                if ap_active:
                    await _run(["systemctl", "start", _HOSTAPD_UNIT], timeout=10)
                    self._clear_ap_flag()
                self._release_lock()
                return {
                    "joined": False,
                    "error": err.strip() or "nmcli_failed",
                    "ip": None,
                    "gateway": None,
                }

            await asyncio.sleep(2.0)
            st = await self.status()
            data = self._load_client_config()
            data["last_ssid"] = ssid
            self._save_client_config(data)
            await self._bus.publish(WifiClientEvent(
                kind="connected",
                ssid=st.get("ssid"),
                signal=st.get("signal"),
                ip=st.get("ip"),
                timestamp_ms=_now_ms(),
            ))
            return {
                "joined": True,
                "error": None,
                "ip": st.get("ip"),
                "gateway": st.get("gateway"),
            }
        except Exception as exc:  # noqa: BLE001
            log.error("wifi_join_exception", error=str(exc))
            self._release_lock()
            return {
                "joined": False,
                "error": f"exception: {exc}",
                "ip": None,
                "gateway": None,
            }

    async def leave(self) -> dict:
        """Disconnect the current WiFi client connection."""
        st = await self.status()
        prev_ssid = st.get("ssid")
        if not prev_ssid:
            self._release_lock()
            return {"left": False, "previous_ssid": None}

        rc, out, err = await _run(
            ["nmcli", "connection", "down", prev_ssid],
            timeout=10,
        )
        if rc != 0:
            # Fallback: disconnect device.
            await _run(["nmcli", "device", "disconnect", self._interface], timeout=10)

        await self._bus.publish(WifiClientEvent(
            kind="disconnected",
            ssid=prev_ssid,
            signal=None,
            ip=None,
            timestamp_ms=_now_ms(),
        ))

        # Restore hostapd if it was running before we took over.
        if self._read_ap_flag():
            log.info("restoring_hostapd_after_client_leave")
            await _run(["systemctl", "start", _HOSTAPD_UNIT], timeout=10)
        self._clear_ap_flag()
        self._release_lock()

        return {"left": True, "previous_ssid": prev_ssid}

    async def status(self) -> dict:
        """Return current station status."""
        rc, out, err = await _run(
            [
                "nmcli", "-t",
                "-f", "ACTIVE,SSID,BSSID,SIGNAL,SECURITY",
                "device", "wifi", "list", "ifname", self._interface,
            ],
            timeout=5,
        )
        active_ssid = None
        bssid = None
        signal_val: int | None = None
        security = None
        if rc == 0:
            for row in _parse_nmcli_terse(out, 5):
                if row[0].strip() == "yes":
                    active_ssid = row[1]
                    bssid = row[2]
                    try:
                        signal_val = int(row[3])
                    except ValueError:
                        signal_val = None
                    security = row[4]
                    break

        ip_addr = None
        gateway = None
        rc2, out2, _ = await _run(
            ["ip", "-4", "addr", "show", self._interface],
            timeout=5,
        )
        if rc2 == 0:
            m = re.search(r"inet\s+(\d+\.\d+\.\d+\.\d+)", out2)
            if m:
                ip_addr = m.group(1)

        rc3, out3, _ = await _run(
            ["ip", "-4", "route", "show", "default", "dev", self._interface],
            timeout=5,
        )
        if rc3 == 0:
            m = re.search(r"default\s+via\s+(\d+\.\d+\.\d+\.\d+)", out3)
            if m:
                gateway = m.group(1)

        return {
            "connected": active_ssid is not None and ip_addr is not None,
            "ssid": active_ssid,
            "bssid": bssid,
            "signal": signal_val,
            "ip": ip_addr,
            "gateway": gateway,
            "security": security,
        }

    async def configured_connections(self) -> list[dict]:
        """Return saved NM WiFi connection profiles."""
        rc, out, err = await _run(
            [
                "nmcli", "-t",
                "-f", "NAME,TYPE,DEVICE,AUTOCONNECT",
                "connection", "show",
            ],
            timeout=10,
        )
        if rc != 0:
            return []
        results: list[dict] = []
        for row in _parse_nmcli_terse(out, 4):
            name, ctype, device, autoconnect = row
            if "wireless" not in ctype:
                continue
            results.append({
                "name": name,
                "type": ctype,
                "device": device or None,
                "autoconnect": autoconnect.strip().lower() == "yes",
            })
        return results

    # -------------------- background poll --------------------

    async def _poll_loop(self, interval_s: float = 10.0) -> None:
        """Poll status periodically and emit signal_changed events."""
        while True:
            try:
                st = await self.status()
                prev = self._last_status
                if prev:
                    if prev.get("connected") and not st.get("connected"):
                        await self._bus.publish(WifiClientEvent(
                            kind="disconnected",
                            ssid=prev.get("ssid"),
                            signal=None,
                            ip=None,
                            timestamp_ms=_now_ms(),
                        ))
                    elif not prev.get("connected") and st.get("connected"):
                        await self._bus.publish(WifiClientEvent(
                            kind="connected",
                            ssid=st.get("ssid"),
                            signal=st.get("signal"),
                            ip=st.get("ip"),
                            timestamp_ms=_now_ms(),
                        ))
                    elif (
                        st.get("connected")
                        and st.get("signal") is not None
                        and prev.get("signal") is not None
                        and abs(st["signal"] - prev["signal"]) >= 10
                    ):
                        await self._bus.publish(WifiClientEvent(
                            kind="signal_changed",
                            ssid=st.get("ssid"),
                            signal=st.get("signal"),
                            ip=st.get("ip"),
                            timestamp_ms=_now_ms(),
                        ))
                self._last_status = st
            except Exception as exc:  # noqa: BLE001
                log.debug("wifi_client_poll_error", error=str(exc))
            await asyncio.sleep(interval_s)

    def start_polling(self) -> None:
        if self._poll_task is None or self._poll_task.done():
            self._poll_task = asyncio.create_task(self._poll_loop())

    def stop_polling(self) -> None:
        if self._poll_task and not self._poll_task.done():
            self._poll_task.cancel()


# -------------------- singleton --------------------


_INSTANCE: WifiClientManager | None = None


def get_wifi_client_manager() -> WifiClientManager:
    global _INSTANCE
    if _INSTANCE is None:
        _INSTANCE = WifiClientManager()
    return _INSTANCE


# -------------------- service entry --------------------


async def main() -> None:
    """Entry point for a future ados-wifi-client.service unit."""
    from ados.core.config import load_config
    from ados.core.logging import configure_logging
    import structlog

    cfg = load_config()
    configure_logging(cfg.logging.level)
    slog = structlog.get_logger()
    slog.info("wifi_client_service_starting")

    mgr = get_wifi_client_manager()

    # Startup check: if a prior session left AP disabled but hostapd is
    # not running, surface the decision to the router (do not auto-start).
    if mgr._read_ap_flag() and not mgr._is_hostapd_active():
        slog.info(
            "wifi_client_startup_ap_flag_lingering",
            hint="uplink_router decides whether to restore AP or keep client",
        )

    cfg_data = mgr._load_client_config()
    if cfg_data.get("enabled_on_boot") and cfg_data.get("last_ssid"):
        slog.info("wifi_client_auto_rejoin_requested", ssid=cfg_data["last_ssid"])

    mgr.start_polling()

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    await shutdown.wait()

    mgr.stop_polling()
    await mgr.bus.close()
    slog.info("wifi_client_service_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
