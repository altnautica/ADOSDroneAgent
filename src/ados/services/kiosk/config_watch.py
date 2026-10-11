"""HDMI kiosk: config readers and the live settings watch."""

from __future__ import annotations

import asyncio
import os
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

from ados.core.config import load_config
from ados.core.paths import CONFIG_YAML
from ados.services.kiosk._log import log

if TYPE_CHECKING:
    from ados.services.kiosk.supervisor import KioskSupervisor

# How often the kiosk re-reads the config file's stat signature to notice an
# operator edit to `ground_station.kiosk.enabled` / `resolution` without a reboot.
_CONFIG_POLL_SECONDS = 3.0

# `ground_station.kiosk.resolution` -> the DRM mode applied to the connected
# output inside cage. Anything else (``auto``, an unknown value) leaves the mode
# the compositor picked alone.
_RESOLUTION_MODES = {"720p": "1280x720", "1080p": "1920x1080"}

# Trailing slash on purpose: it is the path the static mount actually serves,
# so the kiosk never pays a redirect on boot and the query string this module
# appends is not at the mercy of one. `kiosk=1` tells the cockpit it is the
# panel's only surface, so it hides its exit-to-dashboard control.
_DEFAULT_URL = "http://localhost:8080/cockpit/?kiosk=1"
_ENV_URL_KEY = "ADOS_KIOSK_URL"
_ENV_MINIMAL_KEY = "ADOS_KIOSK_MINIMAL_LAYER"


def _display_selection(config: Any) -> str:
    """The operator's `ground_station.display.type`, or ``auto`` when unset.

    Read defensively: this service must start on a config shaped by an older or
    newer agent, and a missing block means "no opinion", not a crash.
    """
    try:
        value = config.ground_station.display.type
    except AttributeError:
        return "auto"
    if not isinstance(value, str):
        return "auto"
    selection = value.strip().lower()
    return selection if selection in ("auto", "hdmi", "lcd", "none") else "auto"


def _get_kiosk_config(config: Any) -> tuple[str | None, bool | None]:
    """Return (target_url, minimal_layer) from config, if present.

    Reads ``config.ground_station.kiosk`` (the KioskConfig model). Accessed
    defensively with ``getattr`` so a duck-typed test config or a bare object
    without the section resolves to ``(None, None)`` instead of raising. Either
    field may be None when unset.
    """
    gs = getattr(config, "ground_station", None)
    if gs is None:
        return None, None
    kiosk = getattr(gs, "kiosk", None)
    if kiosk is None:
        return None, None
    url = getattr(kiosk, "target_url", None)
    minimal = getattr(kiosk, "minimal_layer", None)
    if isinstance(url, str) and not url.strip():
        url = None
    return url, minimal


@dataclass(frozen=True)
class KioskSettings:
    """The kiosk config fields that change what the service runs.

    ``output_mode`` is the DRM mode (``WIDTHxHEIGHT``) the configured
    resolution maps to, or None to leave the compositor's choice alone."""

    enabled: bool
    output_mode: str | None


def _kiosk_settings(config: Any) -> KioskSettings:
    """Read ``ground_station.kiosk.enabled`` / ``resolution``.

    Defensive like the other readers: a missing section or a non-bool
    ``enabled`` reads as disabled (the model default), and a resolution that is
    not one of the known presets (``auto`` included) leaves the mode alone.
    """
    kiosk = getattr(getattr(config, "ground_station", None), "kiosk", None)
    enabled = getattr(kiosk, "enabled", False) is True
    resolution = getattr(kiosk, "resolution", None)
    mode = (
        _RESOLUTION_MODES.get(resolution.strip().lower())
        if isinstance(resolution, str)
        else None
    )
    return KioskSettings(enabled=enabled, output_mode=mode)


def _config_signature() -> tuple[int, int] | None:
    """``(inode, mtime_ns)`` of the config file, or None when it is absent.

    The config writers replace the file atomically, so either half changing
    means a new file landed."""
    try:
        st = os.stat(CONFIG_YAML)
    except OSError:
        return None
    return st.st_ino, st.st_mtime_ns


class _SettingsWatch:
    """Notices an edit to the kiosk settings without a reboot.

    ``poll()`` is cheap when nothing changed (one ``stat``); it reloads the
    config only when the file's signature moved, and reports True only when
    the kiosk settings themselves differ from the ones in force."""

    def __init__(
        self, settings: KioskSettings, signature: tuple[int, int] | None
    ) -> None:
        self.settings = settings
        self._signature = signature

    def poll(self) -> bool:
        signature = _config_signature()
        if signature == self._signature:
            return False
        self._signature = signature
        try:
            config = load_config()
        except Exception as exc:
            log.warning("kiosk_config_reload_failed", error=str(exc))
            return False
        settings = _kiosk_settings(config)
        if settings == self.settings:
            return False
        log.info(
            "kiosk_settings_changed",
            enabled=settings.enabled,
            output_mode=settings.output_mode,
            previous_enabled=self.settings.enabled,
            previous_output_mode=self.settings.output_mode,
        )
        self.settings = settings
        return True


async def _stop_on_settings_change(
    watch: _SettingsWatch, supervisor: KioskSupervisor
) -> None:
    """Stop ``supervisor`` once the kiosk settings change, then return."""
    while True:
        await asyncio.sleep(_CONFIG_POLL_SECONDS)
        if await asyncio.to_thread(watch.poll):
            supervisor.request_stop()
            return


def _resolve_target_url(config: Any) -> tuple[str, bool]:
    """Config -> env -> default. Returns (url_with_query, minimal_flag).

    Query string `?layer=minimal` is appended when either the config
    flag is true or the board has less than 3 GiB RAM. An explicit
    env override `ADOS_KIOSK_MINIMAL_LAYER=0` forces the full layer.
    """
    cfg_url, cfg_minimal = _get_kiosk_config(config)

    url = cfg_url or os.environ.get(_ENV_URL_KEY) or _DEFAULT_URL

    # Minimal by default on the panel. The reduced layer drops the backdrop
    # blur the cockpit draws over live video, and a blurred region above a
    # surface that changes every frame forces the compositor to re-read and
    # re-blur its backdrop at the video's frame rate.
    #
    # Measured on a 4-core panel with video actually arriving over the radio:
    # the full layer costs 292.9% of 400% and leaves 36% of the board idle;
    # the reduced layer costs 137.9% and leaves 68% idle. Same video, same
    # frame rate, 53% less CPU.
    #
    # This used to be gated on the board having under 3 GiB of RAM, which is
    # the wrong quantity: the blur costs compositor time, not memory. A 3.8 GiB
    # four-core panel — comfortably over that threshold — was therefore paying
    # full price for an effect nobody can see behind a HUD, and was the board
    # the reduced path would have helped most. An operator who wants the full
    # layer asks for it.
    minimal = cfg_minimal is not False

    env_minimal = os.environ.get(_ENV_MINIMAL_KEY)
    if env_minimal is not None:
        minimal = env_minimal.strip() not in ("0", "false", "False", "")

    if minimal:
        sep = "&" if "?" in url else "?"
        url = f"{url}{sep}layer=minimal"

    return url, minimal
