"""HDMI kiosk: DRM display probing and cockpit-URL readiness waits."""

from __future__ import annotations

import asyncio
import time
from pathlib import Path
from typing import TYPE_CHECKING

from ados.services.kiosk._log import log

if TYPE_CHECKING:
    from ados.services.kiosk.config_watch import _SettingsWatch

_DRM_CARD_PATH = Path("/dev/dri/card0")
_DRM_DIR = Path("/dev/dri")
_DRM_SYSFS = Path("/sys/class/drm")

# The DRM display devices can appear a few seconds AFTER multi-user/graphical
# is reached at boot (the GPU/KMS driver probes asynchronously), and an operator
# may plug a monitor in at any time after that. We poll for a display at a fixed
# interval for as long as it takes instead of gating on it once or giving up.
_DISPLAY_POLL_SECONDS = 5.0

# The agent HTTP surface that serves the cockpit (the native control front and
# the FastAPI app it proxies) finishes starting a few seconds after this service
# on boot. A browser launched before the URL is served loads an error page and
# STICKS there (no auto-retry), so the operator sees a "404 Not Found" instead of
# the cockpit. We poll the target URL until it is served before launching.
_URL_WAIT_SECONDS = 90.0
_URL_POLL_SECONDS = 1.0
_URL_PROBE_TIMEOUT = 3.0

# Fallback DRM device for the appliance (no-desktop) case when the connected
# card cannot be determined. Prefer `_resolve_drm_device()`, which finds the
# card that actually drives a display — `_hdmi_present` already knows these can
# differ, and handing cage the wrong one leaves it with no connector to modeset.
_DRM_DEVICE = "/dev/dri/card0"


def _hdmi_present() -> bool:
    """True when a DRM display is available.

    Prefers a connector reporting ``connected`` (a real monitor), scanning ALL
    cards — on some boards (e.g. a Raspberry Pi) the render node is ``card0``
    and the display is ``card1``, so a card0-only check is wrong. Falls back to
    "any ``/dev/dri/card*`` node exists" when the sysfs status is unreadable, so
    a board whose DRM subsystem is up but whose connector status we cannot read
    still counts. Absent entirely (headless / DRM not loaded) -> False.
    """
    try:
        for status in _DRM_SYSFS.glob("card*-*/status"):
            try:
                if status.read_text().strip() == "connected":
                    return True
            except OSError:
                continue
    except OSError:
        pass
    try:
        return any(_DRM_DIR.glob("card*"))
    except OSError:
        return False


def _resolve_drm_device() -> str:
    """The DRM card that actually drives a display.

    On a Raspberry Pi 4 the render node is ``card0`` (v3d, no connectors) and
    the display is ``card1`` (vc4). Pinning cage to a hardcoded ``card0`` there
    hands it a device with nothing to modeset, so the appliance path comes up
    with no picture on a board where the browser path works fine.

    Derived from the same connected-connector scan `_hdmi_present` uses, so the
    two cannot disagree about which card is the display. Falls back to
    [`_DRM_DEVICE`] when no connector reports connected, which keeps the
    previous behaviour on a board whose sysfs we cannot read.
    """
    try:
        for status in sorted(_DRM_SYSFS.glob("card*-*/status")):
            try:
                if status.read_text().strip() != "connected":
                    continue
            except OSError:
                continue
            # ".../card1-HDMI-A-1/status" -> "card1"
            card = status.parent.name.split("-", 1)[0]
            node = _DRM_DIR / card
            if node.exists():
                return str(node)
    except OSError:
        pass
    return _DRM_DEVICE


def _connected_outputs() -> dict[str, frozenset[str]]:
    """Connected DRM connectors -> the modes each one advertises.

    Keyed by the connector name without its card prefix
    (``card1-HDMI-A-1`` -> ``HDMI-A-1``), which is the output name wlroots (and
    so cage and ``wlr-randr``) uses. Modes are the ``WIDTHxHEIGHT`` lines of the
    connector's sysfs ``modes`` file.
    """
    outputs: dict[str, frozenset[str]] = {}
    try:
        statuses = sorted(_DRM_SYSFS.glob("card*-*/status"))
    except OSError:
        return outputs
    for status in statuses:
        try:
            if status.read_text().strip() != "connected":
                continue
            modes = (status.parent / "modes").read_text().split()
        except OSError:
            continue
        name = status.parent.name.split("-", 1)[1]
        outputs[name] = frozenset(modes)
    return outputs


async def _wait_for_display(watch: _SettingsWatch) -> bool:
    """Wait for a DRM display to appear, for as long as it takes.

    Absorbs both the boot race (the KMS device is created shortly after the
    service starts) and a monitor hot-plugged at any later time: the display is
    re-probed on a fixed interval with no deadline. Returns True as soon as a
    display is present, or False when the operator disables the kiosk while it
    is waiting (the config is re-checked on the same tick)."""
    if _hdmi_present():
        return True
    log.info("kiosk_waiting_for_display", poll_s=_DISPLAY_POLL_SECONDS)
    while True:
        await asyncio.sleep(_DISPLAY_POLL_SECONDS)
        if await asyncio.to_thread(watch.poll) and not watch.settings.enabled:
            return False
        if _hdmi_present():
            return True


def _url_serving(url: str) -> bool:
    """True when the target URL answers with a non-error HTTP status, i.e. the
    cockpit is being served. A connection refusal or a 4xx/5xx (the proxy up
    before its backend, or the route not mounted yet) reads as not-ready. A
    401/403/405 still means the server is up and routing, so it counts as served
    (the on-box cockpit HTML is trusted and does not gate, but be lenient)."""
    import urllib.error
    import urllib.request

    try:
        with urllib.request.urlopen(url, timeout=_URL_PROBE_TIMEOUT) as resp:  # noqa: S310
            return 200 <= resp.status < 400
    except urllib.error.HTTPError as exc:
        return exc.code in (401, 403, 405)
    except (urllib.error.URLError, OSError, ValueError):
        return False


async def _wait_for_url(url: str) -> bool:
    """Wait (bounded) for the cockpit URL to be served before launching the
    browser, absorbing the boot race where the agent HTTP surface finishes
    starting after this service. Without it the browser loads an error page and
    sticks there. Returns True once served, False after the timeout (launch
    anyway, best-effort — a genuinely-wrong URL should still show something)."""
    if await asyncio.to_thread(_url_serving, url):
        return True
    log.info("kiosk_waiting_for_url", url=url, timeout_s=_URL_WAIT_SECONDS)
    deadline = time.monotonic() + _URL_WAIT_SECONDS
    while time.monotonic() < deadline:
        await asyncio.sleep(_URL_POLL_SECONDS)
        if await asyncio.to_thread(_url_serving, url):
            return True
    log.warning(
        "kiosk_url_wait_timeout",
        url=url,
        msg="cockpit URL not serving after wait; launching browser anyway",
    )
    return False


def hdmi_present() -> bool:
    """Public alias for ``_hdmi_present``.

    The heartbeat enrichment helper imports this to resolve the
    effective display type on boards where ``ground_station.display.type``
    is left at ``auto``. Keeping a public wrapper instead of removing
    the leading-underscore name preserves the in-module call sites that
    use the private form.
    """
    return _hdmi_present()
