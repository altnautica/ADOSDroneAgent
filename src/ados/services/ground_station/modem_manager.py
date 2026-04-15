"""Ground-station modem manager (MSN-027 Wave A).

Wraps the existing `src/ados/services/network/modem_at.py` AT-command
backend and the `src/ados/hal/modem.py` mmcli probe with a
dbus-next-first frontend. The preferred path is ModemManager over
system dbus (`org.freedesktop.ModemManager1`). If dbus is unavailable
or three consecutive dbus calls time out, the manager flips to AT
fallback mode and keeps serving status + data-usage reads.

This module lives in the `ground_station` package because the full
air-side agent uses its own embedded modem driver. The ground station
runs a dedicated `ados-modem.service` unit that owns cellular
uplink for the cloud relay path (DEC-070 MQTT + Convex HTTP).

Public surface:

    manager = GroundStationModemManager()
    await manager.probe()            # -> {detected, device_path, model, imei}
    await manager.bring_up(apn="auto")
    await manager.status()           # -> {connected, iface, ip, signal_quality, ...}
    await manager.data_usage()       # -> {rx_bytes, tx_bytes, total_bytes, last_read}
    await manager.configure(apn, cap_gb, enabled)
    await manager.bring_down()

dbus-next is a lazy import so this module parses cleanly on the
air-side profile where the `ground-station` extra is not installed.
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import signal
import subprocess
import time
from pathlib import Path
from typing import Any, Optional

import structlog

from ados.hal.modem import detect_modem, get_modem_status

log = structlog.get_logger(__name__)

__all__ = [
    "GroundStationModemManager",
    "get_modem_manager",
    "main",
]


_CONFIG_PATH = Path("/etc/ados/ground-station-modem.json")
_WWAN_IFACE = "wwan0"
_USB_IFACE = "usb0"
_DBUS_TIMEOUT_SECONDS = 3.0
_DBUS_FAIL_THRESHOLD = 3

_DEFAULT_APN_FALLBACK = "internet"

# IMSI MCC-MNC -> APN mapping for Indian carriers. MCC 404/405 both
# map to India. Keep this small and maintained rather than pulling a
# full mobile network list into the agent.
_IMSI_APN_MAP: list[tuple[str, str]] = [
    # Jio (Reliance)
    ("405857", "jionet"),
    ("405854", "jionet"),
    ("405855", "jionet"),
    ("405856", "jionet"),
    ("405874", "jionet"),
    # Airtel
    ("40410", "airtelgprs.com"),
    ("40445", "airtelgprs.com"),
    ("40449", "airtelgprs.com"),
    ("40490", "airtelgprs.com"),
    ("40492", "airtelgprs.com"),
    ("40493", "airtelgprs.com"),
    ("40494", "airtelgprs.com"),
    ("40495", "airtelgprs.com"),
    ("40496", "airtelgprs.com"),
    ("40497", "airtelgprs.com"),
    ("40498", "airtelgprs.com"),
    # Vi (Vodafone Idea)
    ("40411", "portalnmms"),
    ("40443", "www"),
    ("40446", "www"),
    # BSNL
    ("40434", "bsnlnet"),
    ("40438", "bsnlnet"),
    ("40451", "bsnlnet"),
    ("40453", "bsnlnet"),
    ("40459", "bsnlnet"),
]


def _atomic_write_json(path: Path, data: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(data), encoding="utf-8")
    os.replace(tmp, path)


def _read_json_safe(path: Path) -> dict:
    try:
        if path.exists():
            return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        log.warning("modem.config_read_failed", path=str(path), error=str(exc))
    return {}


class GroundStationModemManager:
    """Single-modem cellular data manager with dbus-first, AT-fallback.

    Thread-safety: the public async methods are serialized by an
    internal lock so a REST caller and the periodic status loop cannot
    race on the dbus bus handle.
    """

    def __init__(self) -> None:
        self._lock = asyncio.Lock()
        self._dbus_fail_count = 0
        self._fallback_mode = False
        self._bus: Any = None  # dbus-next MessageBus, lazy init
        self._config = _read_json_safe(_CONFIG_PATH)
        self._last_status: dict = {}
        self._brought_up = False

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------
    async def probe(self) -> dict:
        """Detect the modem via mmcli (hal/modem.py).

        Returns a dict describing what was found. The `device_path`
        field is the serial device we would hand to `AtModemService`
        in fallback mode.
        """
        info = await asyncio.to_thread(detect_modem)
        device_path = await self._find_serial_port()
        imei = await self._read_imei_via_dbus()

        detected = info is not None or device_path is not None
        model = info.name if info is not None else None

        result = {
            "detected": detected,
            "device_path": device_path,
            "model": model,
            "imei": imei,
        }
        log.info("modem.probed", **result)
        return result

    async def bring_up(self, apn: str = "auto") -> dict:
        """Start the cellular data session.

        Preferred path: ModemManager dbus (`Modem.Enable` +
        `SimpleModem.Connect`). On dbus timeout or failure, falls
        through to AT commands via the existing `modem_at.py` helpers.
        """
        async with self._lock:
            resolved_apn = apn
            if apn == "auto":
                resolved_apn = await self._auto_detect_apn() or _DEFAULT_APN_FALLBACK

            # Try dbus first.
            if not self._fallback_mode:
                dbus_result = await self._bring_up_dbus(resolved_apn)
                if dbus_result is not None:
                    self._brought_up = True
                    return dbus_result

            # AT fallback.
            at_result = await self._bring_up_at(resolved_apn)
            self._brought_up = at_result.get("connected", False)
            return at_result

    async def bring_down(self) -> dict:
        async with self._lock:
            # Best-effort: try dbus first, then AT, then raw link down.
            ok = False
            if not self._fallback_mode:
                ok = await self._bring_down_dbus()
            if not ok:
                ok = await self._bring_down_link()
            self._brought_up = False
            return {"ok": ok}

    async def status(self) -> dict:
        async with self._lock:
            if not self._fallback_mode:
                dbus_status = await self._status_dbus()
                if dbus_status is not None:
                    self._last_status = dbus_status
                    return dbus_status
            at_status = await self._status_at()
            self._last_status = at_status
            return at_status

    async def data_usage(self) -> dict:
        """Read byte counters from /sys/class/net/wwan0 or usb0."""
        iface = self._current_iface()
        base = Path(f"/sys/class/net/{iface}/statistics")
        rx = tx = 0
        try:
            rx = int((base / "rx_bytes").read_text().strip())
            tx = int((base / "tx_bytes").read_text().strip())
        except OSError:
            return {
                "rx_bytes": 0,
                "tx_bytes": 0,
                "total_bytes": 0,
                "iface": iface,
                "last_read": time.time(),
                "available": False,
            }
        return {
            "rx_bytes": rx,
            "tx_bytes": tx,
            "total_bytes": rx + tx,
            "iface": iface,
            "last_read": time.time(),
            "available": True,
        }

    async def configure(
        self,
        apn: Optional[str] = None,
        cap_gb: Optional[float] = None,
        enabled: Optional[bool] = None,
    ) -> dict:
        async with self._lock:
            current = dict(self._config)
            changed = False
            if apn is not None and apn != current.get("apn"):
                current["apn"] = apn
                changed = True
            if cap_gb is not None and cap_gb != current.get("cap_gb"):
                current["cap_gb"] = cap_gb
                changed = True
            if enabled is not None and enabled != current.get("enabled"):
                current["enabled"] = enabled
                changed = True
            if changed:
                _atomic_write_json(_CONFIG_PATH, current)
                self._config = current
                log.info("modem.config_updated", **current)

        # Apply side effects outside the lock to avoid nesting.
        if changed and (apn is not None or enabled is not None):
            if self._config.get("enabled", True):
                try:
                    await self.bring_up(self._config.get("apn", "auto"))
                except Exception as exc:
                    log.warning("modem.reapply_failed", error=str(exc))
            else:
                try:
                    await self.bring_down()
                except Exception as exc:
                    log.warning("modem.bring_down_failed", error=str(exc))

        return dict(self._config)

    # ------------------------------------------------------------------
    # dbus-next path
    # ------------------------------------------------------------------
    async def _get_bus(self) -> Optional[Any]:
        if self._bus is not None:
            return self._bus
        try:
            from dbus_next.aio import MessageBus  # type: ignore
            from dbus_next import BusType  # type: ignore
        except ImportError as exc:
            log.warning("modem.dbus_unavailable", error=str(exc))
            self._fallback_mode = True
            return None
        try:
            self._bus = await asyncio.wait_for(
                MessageBus(bus_type=BusType.SYSTEM).connect(),
                timeout=_DBUS_TIMEOUT_SECONDS,
            )
            return self._bus
        except Exception as exc:
            self._register_dbus_failure(str(exc))
            return None

    def _register_dbus_failure(self, reason: str) -> None:
        self._dbus_fail_count += 1
        log.warning(
            "modem.dbus_fail",
            count=self._dbus_fail_count,
            reason=reason,
        )
        if self._dbus_fail_count >= _DBUS_FAIL_THRESHOLD and not self._fallback_mode:
            self._fallback_mode = True
            log.warning("modem.fallback_to_at", reason="dbus_errors_exceeded")

    def _register_dbus_success(self) -> None:
        if self._dbus_fail_count > 0:
            self._dbus_fail_count = 0
        if self._fallback_mode:
            self._fallback_mode = False
            log.info("modem.dbus_recovered")

    async def _list_modem_objects(self) -> list[str]:
        """Return dbus object paths under /org/freedesktop/ModemManager1/Modem."""
        bus = await self._get_bus()
        if bus is None:
            return []
        try:
            introspection = await asyncio.wait_for(
                bus.introspect(
                    "org.freedesktop.ModemManager1",
                    "/org/freedesktop/ModemManager1",
                ),
                timeout=_DBUS_TIMEOUT_SECONDS,
            )
            obj = bus.get_proxy_object(
                "org.freedesktop.ModemManager1",
                "/org/freedesktop/ModemManager1",
                introspection,
            )
            iface = obj.get_interface("org.freedesktop.DBus.ObjectManager")
            managed = await asyncio.wait_for(
                iface.call_get_managed_objects(), timeout=_DBUS_TIMEOUT_SECONDS
            )
            paths = [
                p for p in managed.keys()
                if "/ModemManager1/Modem/" in p and "/Bearer/" not in p
            ]
            self._register_dbus_success()
            return paths
        except Exception as exc:
            self._register_dbus_failure(str(exc))
            return []

    async def _bring_up_dbus(self, apn: str) -> Optional[dict]:
        paths = await self._list_modem_objects()
        if not paths:
            return None
        bus = self._bus
        if bus is None:
            return None
        path = paths[0]
        try:
            introspection = await asyncio.wait_for(
                bus.introspect("org.freedesktop.ModemManager1", path),
                timeout=_DBUS_TIMEOUT_SECONDS,
            )
            obj = bus.get_proxy_object(
                "org.freedesktop.ModemManager1", path, introspection
            )
            try:
                modem_iface = obj.get_interface("org.freedesktop.ModemManager1.Modem")
                await asyncio.wait_for(
                    modem_iface.call_enable(True), timeout=_DBUS_TIMEOUT_SECONDS
                )
            except Exception as exc:
                log.debug("modem.dbus_enable_skipped", error=str(exc))

            try:
                simple_iface = obj.get_interface(
                    "org.freedesktop.ModemManager1.Modem.Simple"
                )
            except Exception as exc:
                self._register_dbus_failure(str(exc))
                return None

            from dbus_next import Variant  # type: ignore
            props = {"apn": Variant("s", apn)}
            try:
                await asyncio.wait_for(
                    simple_iface.call_connect(props),
                    timeout=_DBUS_TIMEOUT_SECONDS * 4,
                )
                self._register_dbus_success()
            except Exception as exc:
                self._register_dbus_failure(str(exc))
                return None

            iface_name = self._current_iface()
            ip = await self._read_iface_ip(iface_name)
            return {"connected": True, "iface": iface_name, "ip": ip or "", "apn": apn}
        except Exception as exc:
            self._register_dbus_failure(str(exc))
            return None

    async def _bring_down_dbus(self) -> bool:
        paths = await self._list_modem_objects()
        if not paths or self._bus is None:
            return False
        try:
            introspection = await asyncio.wait_for(
                self._bus.introspect("org.freedesktop.ModemManager1", paths[0]),
                timeout=_DBUS_TIMEOUT_SECONDS,
            )
            obj = self._bus.get_proxy_object(
                "org.freedesktop.ModemManager1", paths[0], introspection
            )
            simple_iface = obj.get_interface(
                "org.freedesktop.ModemManager1.Modem.Simple"
            )
            await asyncio.wait_for(
                simple_iface.call_disconnect("/"),
                timeout=_DBUS_TIMEOUT_SECONDS * 2,
            )
            self._register_dbus_success()
            return True
        except Exception as exc:
            self._register_dbus_failure(str(exc))
            return False

    async def _status_dbus(self) -> Optional[dict]:
        # Use mmcli parsing (hal/modem.py) rather than re-introspecting
        # every property. mmcli goes over dbus under the hood, so we
        # still register failure on exec error.
        info = await asyncio.to_thread(detect_modem)
        if info is None:
            self._register_dbus_failure("mmcli_no_modem")
            return None
        iface = self._current_iface()
        ip = await self._read_iface_ip(iface) or info.ip_address or ""
        technology = await self._read_technology(info.connection_state)
        return {
            "connected": info.connection_state == "connected",
            "iface": iface,
            "ip": ip,
            "signal_quality": int(info.signal_strength),
            "technology": technology,
            "apn": self._config.get("apn", ""),
            "operator": info.operator,
            "fallback_mode": self._fallback_mode,
        }

    async def _read_technology(self, state: str) -> str:
        # Convert mmcli state into a coarse technology label. Finer
        # granularity needs `mmcli -m N --output-keyvalue` parsing,
        # which we skip for now.
        return state if state else "unknown"

    async def _read_imei_via_dbus(self) -> Optional[str]:
        # Lightweight path: parse mmcli -m N for 'equipment identifier'.
        try:
            proc = await asyncio.create_subprocess_exec(
                "mmcli", "-L",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await proc.communicate()
            match = re.search(r"/Modem/(\d+)", stdout.decode(errors="replace"))
            if not match:
                return None
            idx = match.group(1)
            proc = await asyncio.create_subprocess_exec(
                "mmcli", "-m", idx,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await proc.communicate()
            for line in stdout.decode(errors="replace").splitlines():
                low = line.lower()
                if "equipment identifier" in low or "imei" in low:
                    digits = re.findall(r"\d{14,17}", line)
                    if digits:
                        return digits[0]
        except (OSError, FileNotFoundError):
            return None
        return None

    # ------------------------------------------------------------------
    # AT fallback path
    # ------------------------------------------------------------------
    async def _bring_up_at(self, apn: str) -> dict:
        """Run `AtModemService` for a brief connect window.

        We do not keep the `AtModemService` running here in Wave A. We
        invoke its APN set + data enable primitives through a short
        run loop driven by an internal shutdown event. Wave C will
        wire the long-running service unit if we decide to keep the
        AT path as a supervised fallback process.
        """
        from ados.services.network.modem_at import AtModemService

        shutdown = asyncio.Event()
        svc = AtModemService(enabled="true", apn=apn, shutdown_event=shutdown)

        # Kick off the full run loop and give it up to 45 s to either
        # connect or declare failure. We then set the shutdown flag to
        # let it exit cleanly (it keeps polling signal until shutdown).
        async def _deadline() -> None:
            await asyncio.sleep(45.0)
            shutdown.set()

        deadline_task = asyncio.create_task(_deadline())
        svc_task = asyncio.create_task(svc.run())
        try:
            # Poll for the connected flag every second.
            for _ in range(45):
                await asyncio.sleep(1.0)
                if svc.modem_connected:
                    break
            shutdown.set()
            await asyncio.wait_for(svc_task, timeout=5.0)
        except asyncio.TimeoutError:
            svc_task.cancel()
        finally:
            deadline_task.cancel()

        return {
            "connected": bool(svc.modem_connected),
            "iface": _USB_IFACE,
            "ip": svc.modem_ip or "",
            "apn": apn,
            "fallback_mode": True,
        }

    async def _bring_down_link(self) -> bool:
        iface = self._current_iface()
        try:
            proc = await asyncio.create_subprocess_exec(
                "ip", "link", "set", iface, "down",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await proc.communicate()
            return proc.returncode == 0
        except OSError:
            return False

    async def _status_at(self) -> dict:
        """Best-effort status without a live `AtModemService` instance.

        Reads interface up/down + ip from `/sys` and `ip addr`. Signal
        quality is not available without re-opening the modem serial
        port, so it returns -1.
        """
        iface = self._current_iface()
        ip = await self._read_iface_ip(iface) or ""
        up = False
        try:
            operstate = Path(f"/sys/class/net/{iface}/operstate")
            if operstate.exists():
                up = operstate.read_text().strip() == "up"
        except OSError:
            pass
        return {
            "connected": up,
            "iface": iface,
            "ip": ip,
            "signal_quality": -1,
            "technology": "unknown",
            "apn": self._config.get("apn", ""),
            "operator": "",
            "fallback_mode": True,
        }

    # ------------------------------------------------------------------
    # Helpers
    # ------------------------------------------------------------------
    def _current_iface(self) -> str:
        # The SIM7600G-H enumerates as wwan0 under MBIM / QMI and as
        # usb0 under RNDIS + AT. We prefer wwan0 when present.
        if Path(f"/sys/class/net/{_WWAN_IFACE}").exists():
            return _WWAN_IFACE
        if Path(f"/sys/class/net/{_USB_IFACE}").exists():
            return _USB_IFACE
        return _WWAN_IFACE

    async def _read_iface_ip(self, iface: str) -> Optional[str]:
        try:
            proc = await asyncio.create_subprocess_exec(
                "ip", "-4", "addr", "show", iface,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await proc.communicate()
            for line in stdout.decode(errors="replace").splitlines():
                line = line.strip()
                if line.startswith("inet "):
                    return line.split()[1].split("/")[0]
        except OSError:
            return None
        return None

    async def _find_serial_port(self) -> Optional[str]:
        """Reuse `modem_at.AtModemService._find_modem` path detection.

        We do not instantiate the service, just enumerate its known
        ports heuristically. Keeps the call cheap on probe.
        """
        import glob
        candidates = sorted(
            glob.glob("/dev/ttyUSB*") + glob.glob("/dev/ttyACM*")
        )
        if not candidates:
            return None
        # SIM7600G-H exposes AT on ttyUSB2 typically. Return the last
        # ttyUSB as a best-effort default, matching modem_at.py.
        usb_ports = [p for p in candidates if "ttyUSB" in p]
        return usb_ports[-1] if usb_ports else candidates[-1]

    async def _auto_detect_apn(self) -> Optional[str]:
        """Read IMSI via mmcli and pick an APN from the static map."""
        try:
            proc = await asyncio.create_subprocess_exec(
                "mmcli", "-L",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await proc.communicate()
            match = re.search(r"/Modem/(\d+)", stdout.decode(errors="replace"))
            if not match:
                return None
            idx = match.group(1)
            # mmcli -m N --sim-list then mmcli -i SIM gives IMSI.
            proc = await asyncio.create_subprocess_exec(
                "mmcli", "-m", idx,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await proc.communicate()
            text = stdout.decode(errors="replace")
            sim_match = re.search(r"/SIM/(\d+)", text)
            if not sim_match:
                return None
            sim_idx = sim_match.group(1)
            proc = await asyncio.create_subprocess_exec(
                "mmcli", "-i", sim_idx,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await proc.communicate()
            text = stdout.decode(errors="replace")
            imsi_match = re.search(r"imsi\s*:\s*(\d+)", text, flags=re.IGNORECASE)
            if not imsi_match:
                return None
            imsi = imsi_match.group(1)
            for prefix, apn in _IMSI_APN_MAP:
                if imsi.startswith(prefix):
                    log.info(
                        "modem.apn_auto_detected",
                        imsi_prefix=prefix,
                        apn=apn,
                    )
                    return apn
            log.info("modem.apn_no_match", imsi_prefix=imsi[:6])
        except (OSError, FileNotFoundError):
            return None
        return None


# ----------------------------------------------------------------------
# Module-level singleton
# ----------------------------------------------------------------------
_instance: "GroundStationModemManager | None" = None


def get_modem_manager() -> "GroundStationModemManager":
    global _instance
    if _instance is None:
        _instance = GroundStationModemManager()
    return _instance


# ----------------------------------------------------------------------
# Service entry point
# ----------------------------------------------------------------------
async def _run_service() -> None:
    manager = get_modem_manager()
    stop_event = asyncio.Event()

    loop = asyncio.get_running_loop()

    def _handle_signal(signame: str) -> None:
        log.info("modem.signal_received", signal=signame)
        stop_event.set()

    for signame in ("SIGINT", "SIGTERM"):
        try:
            loop.add_signal_handler(
                getattr(signal, signame), _handle_signal, signame
            )
        except NotImplementedError:
            pass

    probe_result = await manager.probe()
    log.info("modem.service_probe", **probe_result)

    if manager._config.get("enabled", True) and probe_result.get("detected"):
        try:
            await manager.bring_up(manager._config.get("apn", "auto"))
        except Exception as exc:
            log.warning("modem.initial_bring_up_failed", error=str(exc))

    log.info("modem.service_ready")

    # Periodic status refresh every 30 s while alive. Keeps the
    # `_last_status` cache warm for cheap reads.
    while not stop_event.is_set():
        try:
            await manager.status()
        except Exception as exc:
            log.warning("modem.status_refresh_error", error=str(exc))
        try:
            await asyncio.wait_for(stop_event.wait(), timeout=30.0)
            break
        except asyncio.TimeoutError:
            pass

    try:
        await manager.bring_down()
    except Exception as exc:
        log.warning("modem.shutdown_bring_down_failed", error=str(exc))
    log.info("modem.service_stopped")


def main() -> None:
    """Systemd entry point for `ados-modem.service`."""
    try:
        asyncio.run(_run_service())
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
