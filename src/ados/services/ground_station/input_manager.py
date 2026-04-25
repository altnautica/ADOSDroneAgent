"""Input device lifecycle for the ground-station profile.

Handles USB and Bluetooth gamepad enumeration, Bluetooth pairing,
primary-device persistence, and hot-plug event publishing. The REST
routes and the PIC arbiter both consume this module by calling
`get_input_manager()` from the `ados-api` process.

Why this file stays light on deps:

* `evdev` is the Linux userspace joystick API. It is only present on
  hosts that installed the ground-station extra. This module must
  parse on every profile, so the import is lazy and localized to the
  methods that actually need it.
* `pyudev` is not a dependency. Hot-plug uses a 1 Hz poll of
  `/dev/input/`. That is good enough for device attach and detach on
  a bench rig, and avoids pulling another library.
* `bluetoothctl` is invoked via `subprocess`. `dbus-next` is declared
  in the ground-station extra and could drive BlueZ directly, but the
  CLI path is simpler, uses the same patterns as `hostapd_manager`,
  and is easy to read from logs.

Lifecycle:

1. Load the persisted primary-device record from
   `/etc/ados/ground-station-input.json` if it exists.
2. Enumerate `/dev/input/event*` on demand and filter to gamepads.
3. Start a long-running hot-plug watcher loop that polls the device
   list every 1 second and publishes `InputDeviceEvent` records on an
   `InputEventBus` whenever devices attach or detach.
4. Expose Bluetooth scan, pair, forget, and paired-list helpers that
   shell out to `bluetoothctl`.

Events published on the bus:

* kind="connected" for a newly seen device.
* kind="disconnected" for a device that dropped off the bus.

Consumers such as `pic_arbiter` subscribe to the bus and react. The
REST routes call `list_gamepads`, `scan_bluetooth`, `pair_bluetooth`,
and friends.

The module exits non-zero from `main()` only on fatal signal. The
hot-plug loop swallows transient errors and keeps running; systemd
restart policy handles crashes.
"""

from __future__ import annotations

import asyncio
import json
import os
import signal
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any, AsyncIterator, Literal

import structlog

from ados.core.config import load_config
from ados.core.logging import configure_logging, get_logger

log = get_logger("ground_station.input")

_STATE_PATH = Path("/etc/ados/ground-station-input.json")
_INPUT_DIR = Path("/dev/input")
_HOTPLUG_POLL_S = 1.0

# Gamepad detection thresholds. A real gamepad has two analog axes and
# a healthy set of buttons. Keyboards and mice fall below the button
# count floor or miss the absolute-axis capability entirely.
_MIN_GAMEPAD_BUTTONS = 8

__all__ = [
    "InputDeviceEvent",
    "InputEventBus",
    "InputManager",
    "get_input_manager",
    "main",
]


@dataclass(frozen=True)
class InputDeviceEvent:
    """One hot-plug observation.

    device_id: stable id (`usb:<vendor>:<product>:<path-basename>` for
        USB, `bt:<mac>` for Bluetooth).
    kind: "connected" or "disconnected".
    name: human-readable device name at the moment of observation.
    path: evdev node path (e.g. `/dev/input/event3`) or empty string
        for Bluetooth disconnect events where the node is already gone.
    device_type: "usb" or "bluetooth".
    timestamp_ms: monotonic milliseconds captured at observation time.
    """

    device_id: str
    kind: Literal["connected", "disconnected"]
    name: str
    path: str
    device_type: Literal["usb", "bluetooth"]
    timestamp_ms: int


class InputEventBus:
    """Asyncio fanout bus for `InputDeviceEvent`.

    Mirrors `ButtonEventBus` in `services/ui/events.py`. Each subscriber
    gets its own queue. Slow subscribers drop their own events, never
    the publisher.
    """

    _SENTINEL: object = object()

    def __init__(self, queue_maxsize: int = 64) -> None:
        self._subscribers: list[asyncio.Queue] = []
        self._queue_maxsize = queue_maxsize
        self._closed = False
        self._lock = asyncio.Lock()

    async def publish(self, event: InputDeviceEvent) -> None:
        if self._closed:
            return
        async with self._lock:
            targets = list(self._subscribers)
        for q in targets:
            try:
                q.put_nowait(event)
            except asyncio.QueueFull:
                pass

    async def subscribe(self) -> AsyncIterator[InputDeviceEvent]:
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
                assert isinstance(item, InputDeviceEvent)
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
            try:
                q.put_nowait(self._SENTINEL)
            except asyncio.QueueFull:
                try:
                    q.get_nowait()
                    q.put_nowait(self._SENTINEL)
                except Exception:
                    pass


def _now_ms() -> int:
    return int(asyncio.get_event_loop().time() * 1000)


def _device_id_for_usb(vendor: int, product: int, path: str) -> str:
    base = os.path.basename(path) if path else "unknown"
    return f"usb:{vendor:04x}:{product:04x}:{base}"


def _device_id_for_bt(mac: str) -> str:
    return f"bt:{mac.lower()}"


class InputManager:
    """USB + Bluetooth gamepad lifecycle for the ground-station profile.

    One instance per agent process, retrieved via `get_input_manager()`.
    The class holds no IO resources across method calls. Hot-plug state
    lives in `self._last_seen` and is updated by `run_hotplug_watcher`.
    """

    def __init__(self) -> None:
        self._primary: str | None = None
        self._last_seen: dict[str, dict[str, Any]] = {}
        self.bus = InputEventBus()
        self._load_state()

    # ------------------------------------------------------------------
    # Primary-device persistence
    # ------------------------------------------------------------------

    def _load_state(self) -> None:
        if not _STATE_PATH.exists():
            log.debug("input_state_missing", path=str(_STATE_PATH))
            return
        try:
            data = json.loads(_STATE_PATH.read_text(encoding="utf-8"))
            primary = data.get("primary")
            if isinstance(primary, str) and primary:
                self._primary = primary
                log.info("input_primary_loaded", device_id=primary)
        except (OSError, json.JSONDecodeError) as exc:
            log.warning(
                "input_state_read_failed",
                path=str(_STATE_PATH),
                error=str(exc),
            )

    def _save_state(self) -> None:
        try:
            _STATE_PATH.parent.mkdir(parents=True, exist_ok=True)
            payload = json.dumps({"primary": self._primary}, indent=2)
            fd, tmp_path = tempfile.mkstemp(
                prefix=".gs-input-", dir=str(_STATE_PATH.parent)
            )
            try:
                with os.fdopen(fd, "w", encoding="utf-8") as f:
                    f.write(payload)
                    f.write("\n")
                os.chmod(tmp_path, 0o644)
                os.replace(tmp_path, _STATE_PATH)
            except Exception:
                try:
                    os.unlink(tmp_path)
                except OSError:
                    pass
                raise
        except OSError as exc:
            log.error(
                "input_state_write_failed",
                path=str(_STATE_PATH),
                error=str(exc),
            )

    def set_primary(self, device_id: str) -> None:
        self._primary = device_id
        self._save_state()
        log.info("input_primary_set", device_id=device_id)

    def get_primary(self) -> str | None:
        return self._primary

    # ------------------------------------------------------------------
    # USB gamepad enumeration (lazy evdev import)
    # ------------------------------------------------------------------

    def _enumerate_evdev(self) -> list[dict[str, Any]]:
        """Return one dict per attached gamepad. Returns [] if evdev is
        not importable. Caller is responsible for swallowing the empty
        list as "no USB gamepads available".
        """
        try:
            import evdev  # type: ignore
            from evdev import ecodes  # type: ignore
        except ImportError:
            log.debug("evdev_not_installed")
            return []

        results: list[dict[str, Any]] = []
        try:
            paths = evdev.list_devices()
        except OSError as exc:
            log.warning("evdev_list_failed", error=str(exc))
            return []

        for path in paths:
            try:
                dev = evdev.InputDevice(path)
            except OSError as exc:
                log.debug("evdev_open_failed", path=path, error=str(exc))
                continue

            try:
                caps = dev.capabilities()
            except OSError as exc:
                log.debug("evdev_caps_failed", path=path, error=str(exc))
                dev.close()
                continue

            abs_codes = [code for code, _ in caps.get(ecodes.EV_ABS, [])]
            key_codes = list(caps.get(ecodes.EV_KEY, []))

            has_axes = (
                ecodes.ABS_X in abs_codes and ecodes.ABS_Y in abs_codes
            )
            has_buttons = len(key_codes) >= _MIN_GAMEPAD_BUTTONS

            if not (has_axes and has_buttons):
                dev.close()
                continue

            vendor = int(getattr(dev.info, "vendor", 0) or 0)
            product = int(getattr(dev.info, "product", 0) or 0)
            name = dev.name or "unknown"
            device_id = _device_id_for_usb(vendor, product, path)

            results.append(
                {
                    "device_id": device_id,
                    "name": name,
                    "path": path,
                    "vendor": vendor,
                    "product": product,
                    "type": "usb",
                    "connected": True,
                }
            )
            dev.close()

        return results

    async def list_gamepads(self) -> list[dict[str, Any]]:
        """Return the current enumeration. Never raises."""
        loop = asyncio.get_event_loop()
        return await loop.run_in_executor(None, self._enumerate_evdev)

    # ------------------------------------------------------------------
    # Bluetooth via bluetoothctl
    # ------------------------------------------------------------------

    async def _btctl(
        self, *args: str, timeout: float = 10.0
    ) -> tuple[int, str, str]:
        """Run `bluetoothctl <args>`. Returns (rc, stdout, stderr)."""
        try:
            proc = await asyncio.create_subprocess_exec(
                "bluetoothctl",
                *args,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
        except (OSError, FileNotFoundError) as exc:
            log.warning("btctl_spawn_failed", args=args, error=str(exc))
            return 127, "", str(exc)

        try:
            stdout_b, stderr_b = await asyncio.wait_for(
                proc.communicate(), timeout=timeout
            )
        except asyncio.TimeoutError:
            log.warning("btctl_timeout", args=args, timeout=timeout)
            try:
                proc.kill()
            except ProcessLookupError:
                pass
            try:
                await asyncio.wait_for(proc.wait(), timeout=1.0)
            except asyncio.TimeoutError:
                pass
            return 124, "", "timeout"

        stdout = stdout_b.decode(errors="replace")
        stderr = stderr_b.decode(errors="replace")
        return proc.returncode or 0, stdout, stderr

    def _parse_bt_device_lines(self, text: str) -> list[dict[str, str]]:
        """Parse `Device <MAC> <Name>` lines from bluetoothctl output."""
        devices: list[dict[str, str]] = []
        for raw in text.splitlines():
            line = raw.strip()
            if not line.startswith("Device "):
                continue
            parts = line.split(None, 2)
            if len(parts) < 2:
                continue
            mac = parts[1].strip()
            name = parts[2].strip() if len(parts) >= 3 else mac
            devices.append({"mac": mac, "name": name})
        return devices

    async def scan_bluetooth(self, duration_s: int = 10) -> list[dict[str, Any]]:
        """Turn on scanning, wait, then list discovered devices.

        Scan is always stopped before returning, even on error. RSSI is
        not surfaced by `bluetoothctl devices`; callers that need RSSI
        can read BlueZ directly later. The name and MAC are enough to
        populate the setup UI.
        """
        await self._btctl("power", "on", timeout=5.0)
        await self._btctl("scan", "on", timeout=5.0)
        try:
            await asyncio.sleep(max(1, int(duration_s)))
            rc, stdout, _ = await self._btctl("devices", timeout=5.0)
        finally:
            await self._btctl("scan", "off", timeout=5.0)

        if rc != 0:
            return []

        devices = self._parse_bt_device_lines(stdout)
        return [
            {"mac": d["mac"], "name": d["name"], "rssi": None} for d in devices
        ]

    async def pair_bluetooth(self, mac: str) -> dict[str, Any]:
        """Pair, trust, and connect a Bluetooth device.

        Each sub-step is logged. If any step fails the overall result
        is `paired=False` with the failing stage in `error`. `trust`
        alone failing is treated as a soft warning because some devices
        pair and connect fine without the explicit trust call.
        """
        mac_norm = mac.strip().upper()
        log.info("bt_pair_start", mac=mac_norm)

        rc, out, err = await self._btctl("pair", mac_norm, timeout=30.0)
        if rc != 0:
            log.warning(
                "bt_pair_failed", mac=mac_norm, rc=rc, stdout=out, stderr=err
            )
            return {"paired": False, "error": f"pair rc={rc}: {err.strip()}"}

        rc_t, out_t, err_t = await self._btctl(
            "trust", mac_norm, timeout=5.0
        )
        if rc_t != 0:
            log.warning(
                "bt_trust_soft_fail",
                mac=mac_norm,
                rc=rc_t,
                stderr=err_t.strip(),
            )

        rc_c, out_c, err_c = await self._btctl(
            "connect", mac_norm, timeout=15.0
        )
        if rc_c != 0:
            log.warning(
                "bt_connect_failed",
                mac=mac_norm,
                rc=rc_c,
                stdout=out_c,
                stderr=err_c,
            )
            return {
                "paired": True,
                "connected": False,
                "error": f"connect rc={rc_c}: {err_c.strip()}",
            }

        log.info("bt_pair_ok", mac=mac_norm)
        return {"paired": True, "connected": True, "error": None}

    async def forget_bluetooth(self, mac: str) -> dict[str, Any]:
        mac_norm = mac.strip().upper()
        log.info("bt_forget_start", mac=mac_norm)

        await self._btctl("disconnect", mac_norm, timeout=5.0)
        rc, _, err = await self._btctl("remove", mac_norm, timeout=5.0)

        if rc != 0:
            log.warning(
                "bt_forget_failed", mac=mac_norm, rc=rc, stderr=err.strip()
            )
            return {"forgotten": False, "error": err.strip() or f"rc={rc}"}

        # Drop any persisted primary assignment that pointed here.
        primary = self.get_primary()
        if primary == _device_id_for_bt(mac_norm):
            self._primary = None
            self._save_state()
            log.info("bt_forget_cleared_primary", mac=mac_norm)

        log.info("bt_forget_ok", mac=mac_norm)
        return {"forgotten": True, "error": None}

    async def paired_bluetooth(self) -> list[dict[str, Any]]:
        rc, stdout, _ = await self._btctl("paired-devices", timeout=5.0)
        if rc != 0:
            return []

        out: list[dict[str, Any]] = []
        for d in self._parse_bt_device_lines(stdout):
            mac = d["mac"]
            out.append(
                {
                    "device_id": _device_id_for_bt(mac),
                    "mac": mac,
                    "name": d["name"],
                    "type": "bluetooth",
                    "connected": True,
                }
            )
        return out

    # ------------------------------------------------------------------
    # Hot-plug watcher (1 Hz polling)
    # ------------------------------------------------------------------

    async def _snapshot(self) -> dict[str, dict[str, Any]]:
        """Return {device_id: record} for the current attached set."""
        usb = await self.list_gamepads()
        snap: dict[str, dict[str, Any]] = {}
        for dev in usb:
            snap[dev["device_id"]] = dev
        return snap

    async def run_hotplug_watcher(self) -> None:
        """Long-running loop. Polls every second, publishes diff events.

        First pass seeds `self._last_seen` without publishing duplicate
        connect events for devices already attached when the agent
        started. If no primary is set yet and a USB gamepad shows up,
        it is auto-promoted to primary.
        """
        log.info("input_hotplug_watcher_started")

        first_pass = True
        while True:
            try:
                snap = await self._snapshot()
            except Exception as exc:
                log.warning("input_snapshot_failed", error=str(exc))
                await asyncio.sleep(_HOTPLUG_POLL_S)
                continue

            prev_ids = set(self._last_seen.keys())
            new_ids = set(snap.keys())

            added = new_ids - prev_ids
            removed = prev_ids - new_ids

            if not first_pass:
                for dev_id in added:
                    rec = snap[dev_id]
                    await self.bus.publish(
                        InputDeviceEvent(
                            device_id=dev_id,
                            kind="connected",
                            name=rec.get("name", ""),
                            path=rec.get("path", ""),
                            device_type="usb",
                            timestamp_ms=_now_ms(),
                        )
                    )
                    log.info(
                        "input_device_connected",
                        device_id=dev_id,
                        name=rec.get("name", ""),
                    )

                for dev_id in removed:
                    rec = self._last_seen.get(dev_id, {})
                    await self.bus.publish(
                        InputDeviceEvent(
                            device_id=dev_id,
                            kind="disconnected",
                            name=rec.get("name", ""),
                            path="",
                            device_type="usb",
                            timestamp_ms=_now_ms(),
                        )
                    )
                    log.info("input_device_disconnected", device_id=dev_id)

            # Auto-promote first USB gamepad to primary on first sight.
            if self._primary is None and snap:
                first_dev_id = next(iter(snap.keys()))
                self.set_primary(first_dev_id)
                log.info(
                    "input_primary_auto_assigned", device_id=first_dev_id
                )

            self._last_seen = snap
            first_pass = False
            await asyncio.sleep(_HOTPLUG_POLL_S)


# ----------------------------------------------------------------------
# Module-level singleton
# ----------------------------------------------------------------------

_instance: "InputManager | None" = None


def get_input_manager() -> "InputManager":
    """Return the process-wide `InputManager` singleton.

    Creates it on first call. Safe to call from REST route handlers,
    the PIC arbiter, and tests. The instance holds only the last-seen
    snapshot plus the event bus, so cross-process state is handled by
    the persisted JSON file, not by the singleton itself.
    """
    global _instance
    if _instance is None:
        _instance = InputManager()
    return _instance


# ----------------------------------------------------------------------
# Service entry point
# ----------------------------------------------------------------------


async def main() -> None:
    """Service entry point. Invoked by systemd via `python -m`."""
    config = load_config()
    configure_logging(config.logging.level)
    slog = structlog.get_logger()
    slog.info("ground_input_service_starting")

    if os.geteuid() != 0:
        slog.warning(
            "ground_input_non_root",
            msg="Input operations expect root, continuing anyway",
        )

    manager = get_input_manager()

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    watcher_task = asyncio.create_task(manager.run_hotplug_watcher())

    slog.info(
        "ground_input_service_ready",
        primary=manager.get_primary(),
    )

    await shutdown.wait()
    slog.info("ground_input_service_stopping")

    watcher_task.cancel()
    try:
        await watcher_task
    except asyncio.CancelledError:
        pass

    await manager.bus.close()
    slog.info("ground_input_service_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
