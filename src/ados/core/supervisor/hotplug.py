"""Hot-plug handling mixin for the Supervisor class.

Lifted out of the lifecycle module so the USB add/remove → systemd
restart routing can be read on its own. The mixin reuses Supervisor
state set up in __init__: `_hotplug_*` fields, `_services`, and
`restart_service`.
"""

from __future__ import annotations

import asyncio
import time

import structlog

from ados.hal.usb import UsbCategory, UsbDevice

log = structlog.get_logger()


class HotplugMixin:
    """Hot-plug event routing for the Supervisor."""

    async def _run_hotplug_monitor(self) -> None:
        """Wrapper around HotplugMonitor.run() with first-scan gating.

        The monitor's first scan fires "add" events for every USB device
        that was already present at supervisor startup. We do not want to
        cascade-restart services for those. The gate flips open after
        ~1.5× the first poll interval, which guarantees the first scan
        finished.
        """
        if self._hotplug_monitor is None:
            return

        interval = self._hotplug_monitor._interval  # noqa: SLF001
        asyncio.create_task(self._hotplug_enable_gate(interval * 1.5))

        try:
            await self._hotplug_monitor.run(self._on_hotplug_event)
        except asyncio.CancelledError:
            log.info("hotplug_monitor_task_cancelled")
            raise
        except Exception as exc:
            log.error("hotplug_monitor_crashed", error=str(exc))

    async def _hotplug_enable_gate(self, delay: float) -> None:
        """Open the hot-plug event gate after the first scan finishes."""
        await asyncio.sleep(delay)
        self._hotplug_first_scan_done = True
        known = (
            len(self._hotplug_monitor.known_devices)
            if self._hotplug_monitor
            else 0
        )
        log.info("hotplug_gate_open", known_devices=known)

    def _on_hotplug_event(self, event: str, device: UsbDevice) -> None:
        """Dispatch USB add/remove events to the right service handlers.

        CRITICAL: structlog uses `event` as the reserved positional
        argument for the log message. Using `event=...` as a kwarg in
        log.info() raises `TypeError: got multiple values for argument
        'event'`. All log calls here rename the payload kwarg to
        `action=` to avoid the collision.
        """
        # First-scan gate
        if not self._hotplug_first_scan_done:
            log.debug(
                "hotplug_event_pre_gate",
                action=event,
                device_name=device.name,
                category=device.category.value,
            )
            return

        # Debounce: coalesce rapid events on the same device (e.g.
        # SpeedyBee DFU→flight transition fires remove+add within ~1s).
        key = f"{device.vid:04x}:{device.pid:04x}"
        now = time.monotonic()
        last = self._hotplug_last_event_time.get(key, 0.0)
        if now - last < self._hotplug_debounce_secs:
            log.debug(
                "hotplug_event_debounced",
                action=event,
                device_name=device.name,
                category=device.category.value,
                delta_secs=round(now - last, 2),
            )
            self._hotplug_last_event_time[key] = now
            return
        self._hotplug_last_event_time[key] = now

        log.info(
            "hotplug_event",
            action=event,
            device_name=device.name,
            vid=f"{device.vid:04x}",
            pid=f"{device.pid:04x}",
            category=device.category.value,
        )

        # Route by device category → service restart
        affected_service: str | None = None
        if device.category == UsbCategory.CAMERA:
            affected_service = "ados-video"
        elif device.category == UsbCategory.FC:
            affected_service = "ados-mavlink"
        elif device.category == UsbCategory.RADIO:
            # Only restart ados-wfb for the WFB-ng adapter
            pid_hex = f"{device.pid:04x}".lower()
            if pid_hex in ("8812", "881a", "881b", "881c", "b812"):  # RTL8812 family
                affected_service = "ados-wfb"
        # GPS / LORA / OTHER: log-only, no restart

        if affected_service and affected_service in self._services:
            log.info(
                "hotplug_triggered_restart",
                service=affected_service,
                action=event,
                device_name=device.name,
            )
            self._schedule_hotplug_restart(affected_service)

    def _schedule_hotplug_restart(self, name: str) -> None:
        """Cancel any pending restart for `name`, then schedule a new one.

        Multiple hot-plug events for the same service within a single
        500ms kernel-settle window collapse into one restart instead of
        thrashing systemctl. Per-service tracking. Different services
        run their restarts concurrently.
        """
        existing = self._hotplug_restart_tasks.get(name)
        if existing is not None and not existing.done():
            existing.cancel()
            log.debug("hotplug_restart_coalesced", service=name)
        task = asyncio.create_task(
            self._hotplug_restart_service(name),
            name=f"hotplug-restart-{name}",
        )
        self._hotplug_restart_tasks[name] = task

    async def _hotplug_restart_service(self, name: str) -> None:
        """Restart a service after a hot-plug event."""
        try:
            # Small delay so the kernel finishes device-node creation.
            # Held in a single sleep so a coalesce-cancel during the
            # window short-circuits the entire restart cleanly.
            await asyncio.sleep(0.5)
            await self.restart_service(name)
            log.info("hotplug_service_restarted", service=name)
        except asyncio.CancelledError:
            # Coalesced by a newer event; nothing to roll back.
            raise
        except Exception as exc:
            log.error(
                "hotplug_service_restart_failed",
                service=name,
                error=str(exc),
            )
        finally:
            # Drop our entry only if it's still us. A successor may have
            # replaced our slot already.
            current = self._hotplug_restart_tasks.get(name)
            if current is asyncio.current_task():
                self._hotplug_restart_tasks.pop(name, None)
