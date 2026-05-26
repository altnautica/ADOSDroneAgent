"""Boot-time display presence probe with apply-verify-auto-revert.

Fires once per boot (as a systemd oneshot) only while a display overlay is
on probation: an SPI-LCD device-tree overlay was written to the boot config
by ``install-display-overlay.sh`` before the panel could be confirmed
present. A boot-critical overlay applied against absent hardware can leave a
headless board unable to bring up its UI surface, and the modules-load +
overlay re-apply every boot so a power cycle alone does not recover it. This
probe makes that apply self-healing:

* CONFIRM: the panel bound this boot (a framebuffer reports the expected
  fbtft driver name and, when the panel has touch, the touch controller
  shows up as an input device). Clear probation, write the persistent
  ``display.enabled`` marker. The overlay stays.

* AUTO-REVERT: the panel never bound. Restore the boot config from the
  snapshot the installer saved, set ``display_id=none``, remove the
  ``display.enabled`` marker, and clear probation. On the next boot the
  board comes up exactly as it did before the blind apply.

The probe touches the boot config ONLY to restore a known-good snapshot, and
only on the revert path. It never writes a new boot-critical overlay. It is
a no-op when no probation marker exists (its systemd unit's
``ConditionPathExists`` already gates that, this is belt-and-suspenders).
"""

from __future__ import annotations

import time
from pathlib import Path

from ados.core.logging import configure_logging, get_logger
from ados.core.paths import (
    DISPLAY_CONF_PATH,
    DISPLAY_ENABLED_PATH,
    DISPLAY_PROBATION_PATH,
)
from ados.setup.hardware_check import _SPI_LCD_DRIVER_NAMES

log = get_logger("display_probe")

# How long to wait for the framebuffer console to bind the SPI LCD. fbtft +
# the panel driver bind late in kernel boot; on the slowest observed board
# (Rock 5C Lite) the bind lands several seconds after local-fs. Poll past
# that worst case before judging the panel absent so we never revert a panel
# that simply bound slowly.
_BIND_POLL_SECONDS = 20.0
_BIND_POLL_INTERVAL = 0.5

# sysfs roots. Module-level so a test can monkeypatch them at a temp tree.
SYS_GRAPHICS_DIR = Path("/sys/class/graphics")
SYS_INPUT_DIR = Path("/sys/class/input")


def _parse_marker(path: Path) -> dict[str, str]:
    """Parse the simple key=value probation / display marker file."""
    out: dict[str, str] = {}
    try:
        text = path.read_text()
    except OSError:
        return out
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, _, v = line.partition("=")
        out[k.strip()] = v.strip()
    return out


def _fb_bound(expected_name: str) -> str | None:
    """Return the fb device name whose driver matches the expected panel.

    Walks every ``/sys/class/graphics/fb*`` and matches by driver NAME, not
    by index: the SPI LCD lands on ``fb0`` when no DRM/HDMI driver claims a
    node (headless) and on ``fb1`` when one does. Prefers the configured
    expected name; falls back to any known SPI-LCD driver name.
    """
    if not SYS_GRAPHICS_DIR.is_dir():
        return None
    fallback: str | None = None
    for entry in sorted(SYS_GRAPHICS_DIR.glob("fb*")):
        if not entry.name.startswith("fb") or not entry.name[2:].isdigit():
            continue
        name_file = entry / "name"
        try:
            fb_name = name_file.read_text().strip()
        except OSError:
            continue
        if expected_name and expected_name in fb_name:
            return entry.name
        if fb_name in _SPI_LCD_DRIVER_NAMES:
            fallback = entry.name
    return fallback


def _touch_bound(touch_chip: str) -> bool:
    """Is the panel's resistive touch controller present as an input device?

    Returns ``True`` when no touch chip is expected (presence of the
    framebuffer alone confirms the panel) or when a matching input device is
    found under ``/sys/class/input/event*/device/name``.
    """
    token = touch_chip.strip().lower()
    if not token:
        return True
    if not SYS_INPUT_DIR.is_dir():
        return False
    for entry in sorted(SYS_INPUT_DIR.glob("event*")):
        name_file = entry / "device" / "name"
        try:
            dev_name = name_file.read_text().strip().lower()
        except OSError:
            continue
        if token in dev_name:
            return True
    return False


def _panel_present(expected_name: str, touch_chip: str) -> str | None:
    """Confirm the panel: matched framebuffer AND (if any) the touch chip.

    Returns the matched fb device name on success, else ``None``.
    """
    fb = _fb_bound(expected_name)
    if fb is None:
        return None
    if not _touch_bound(touch_chip):
        return None
    return fb


def _wait_for_panel(expected_name: str, touch_chip: str) -> str | None:
    """Poll for the panel to bind, up to the late-bind window."""
    deadline = time.monotonic() + _BIND_POLL_SECONDS
    while True:
        fb = _panel_present(expected_name, touch_chip)
        if fb is not None:
            return fb
        if time.monotonic() >= deadline:
            return None
        time.sleep(_BIND_POLL_INTERVAL)


def _confirm(marker: dict[str, str], fb: str) -> None:
    """Panel confirmed: clear probation, write the persistent marker."""
    try:
        DISPLAY_ENABLED_PATH.parent.mkdir(parents=True, exist_ok=True)
        DISPLAY_ENABLED_PATH.write_text("")
    except OSError as exc:
        log.warning("display_probe_marker_write_failed", error=str(exc))
    try:
        DISPLAY_PROBATION_PATH.unlink()
    except OSError:
        pass
    log.info(
        "display_probe_confirmed",
        display_id=marker.get("display_id", "?"),
        framebuffer=fb,
        msg="panel bound; cleared probation, overlay retained",
    )


def _revert(marker: dict[str, str]) -> None:
    """Panel never bound: restore the boot-config snapshot, disable display.

    Restores the saved boot config (the only boot-critical write the probe
    ever makes, and only to a known-good snapshot), sets display.conf to
    display_id=none, removes the persistent marker, and clears probation.
    """
    snapshot = marker.get("snapshot", "").strip()
    boot_config = marker.get("boot_config", "").strip()
    restored = False
    if snapshot and boot_config:
        snap_path = Path(snapshot)
        boot_path = Path(boot_config)
        try:
            if snap_path.is_file():
                # Sanity: never restore an empty/truncated snapshot over a
                # working boot config. A valid extlinux.conf is > 100 bytes.
                data = snap_path.read_bytes()
                if len(data) >= 100:
                    boot_path.write_bytes(data)
                    restored = True
                else:
                    log.warning(
                        "display_probe_snapshot_too_small",
                        bytes=len(data),
                        snapshot=snapshot,
                    )
        except OSError as exc:
            log.warning("display_probe_restore_failed", error=str(exc))

    # Rewrite display.conf to the disabled state so the UI service + heartbeat
    # see "no display" consistently.
    try:
        DISPLAY_CONF_PATH.parent.mkdir(parents=True, exist_ok=True)
        DISPLAY_CONF_PATH.write_text(
            "# Written by ados.services.display_probe after an unconfirmed\n"
            "# SPI-LCD overlay failed to bind. The boot config was restored\n"
            "# from the install-time snapshot and the display disabled.\n"
            "display_id=none\n"
            f"board={marker.get('board', '')}\n"
            "has_touch=false\n"
            "display_presence=reverted\n"
        )
    except OSError as exc:
        log.warning("display_probe_conf_write_failed", error=str(exc))

    try:
        DISPLAY_ENABLED_PATH.unlink()
    except OSError:
        pass
    try:
        DISPLAY_PROBATION_PATH.unlink()
    except OSError:
        pass

    log.warning(
        "display_probe_reverted",
        display_id=marker.get("display_id", "?"),
        boot_config_restored=restored,
        snapshot=snapshot or None,
        msg=(
            "panel did not bind; restored boot config and disabled display"
            if restored
            else "panel did not bind; no snapshot to restore, display disabled"
        ),
    )


def run() -> int:
    """Confirm or auto-revert a probationary display overlay. Returns 0."""
    if not DISPLAY_PROBATION_PATH.exists():
        # No probation in effect. The systemd ConditionPathExists already
        # gates this, but stay a clean no-op if invoked directly.
        log.info("display_probe_noop", msg="no probation marker")
        return 0

    marker = _parse_marker(DISPLAY_PROBATION_PATH)
    expected_name = marker.get("expected_fb_name", "") or "fb_ili9486"
    touch_chip = marker.get("touch_chip", "")
    log.info(
        "display_probe_start",
        display_id=marker.get("display_id", "?"),
        expected_fb_name=expected_name,
        touch_chip=touch_chip or None,
    )

    fb = _wait_for_panel(expected_name, touch_chip)
    if fb is not None:
        _confirm(marker, fb)
    else:
        _revert(marker)
    return 0


def main() -> None:
    configure_logging()
    raise SystemExit(run())


if __name__ == "__main__":
    main()
