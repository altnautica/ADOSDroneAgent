"""HDMI kiosk service: Chromium full-screen, pointed at the agent cockpit.

HDMI + touch + gamepad should deliver a standalone field console with no
laptop required. This service owns the HDMI output. The default target is the
agent-served cockpit at ``http://localhost:8080/cockpit`` (a light SPA served
by the agent's own front, not a Next.js build on the box).

Lifecycle:
1. Honour the operator's config: ``ground_station.display.type`` of ``lcd`` /
   ``none`` or ``ground_station.kiosk.enabled: false`` stands the service down
   with exit 0 (the unit treats that as "nothing to run").
2. Wait for a DRM display, polling for HDMI hotplug at a fixed interval for as
   long as it takes, so a monitor plugged in long after boot still gets the
   cockpit. The rest of the ground station keeps working without HDMI.
3. Resolve target URL via config -> env var -> default chain.
4. Launch Chromium full-screen, adaptively:
   - When a graphical desktop session is already running on the box (a
     display manager with KDE / GNOME / etc.), launch Chromium as a
     full-screen kiosk window INSIDE that session. cage is NOT used here:
     it needs to own the DRM master, which the running desktop compositor
     already holds, so cage would fight the desktop and churn.
   - When no desktop is present (the appliance case), launch under `cage`,
     a Wayland single-app compositor that owns the display itself.
   The Chromium binary is resolved at runtime (its name varies by distro).
   On the cage path a ``ground_station.kiosk.resolution`` of ``720p`` /
   ``1080p`` sets the output mode via ``wlr-randr``; ``auto`` leaves it alone.
5. Supervise the child. On exit, retry on a fixed short interval. Five
   crashes in 60 seconds either downgrades a GPU launch to software or ends
   the service with a non-zero code so systemd restarts it (RestartSec=3).
   The config file is re-checked on a fixed cadence: a change to ``enabled``
   or ``resolution`` restarts the child with the new settings (or stands the
   service down when the kiosk was disabled).
6. On SIGTERM: send SIGTERM to the child, wait 10 s for graceful exit,
   SIGKILL if it is still up. Under cage we also sweep orphaned cage /
   chromium processes; inside a running desktop we do NOT broad-sweep
   chromium (that would kill the operator's own browser windows).

Not in scope:
- Bundling the cockpit here. It is served by the agent front at :8080.
- Sub-30 ms DRM-composited low-latency video (a v2 optimization).
"""

import asyncio
import signal
import sys
from typing import Any

import structlog

from ados.core.config import load_config
from ados.core.logging import configure_logging
from ados.core.paths import ADOS_RUN_DIR as ADOS_RUN_DIR
from ados.services.kiosk._log import log as log
from ados.services.kiosk.config_watch import (
    _CONFIG_POLL_SECONDS as _CONFIG_POLL_SECONDS,
)
from ados.services.kiosk.config_watch import (
    _DEFAULT_URL as _DEFAULT_URL,
)
from ados.services.kiosk.config_watch import (
    _ENV_MINIMAL_KEY as _ENV_MINIMAL_KEY,
)
from ados.services.kiosk.config_watch import (
    _ENV_URL_KEY as _ENV_URL_KEY,
)
from ados.services.kiosk.config_watch import (
    _RESOLUTION_MODES as _RESOLUTION_MODES,
)
from ados.services.kiosk.config_watch import (
    KioskSettings as KioskSettings,
)
from ados.services.kiosk.config_watch import (
    _config_signature as _config_signature,
)
from ados.services.kiosk.config_watch import (
    _display_selection as _display_selection,
)
from ados.services.kiosk.config_watch import (
    _get_kiosk_config as _get_kiosk_config,
)
from ados.services.kiosk.config_watch import (
    _kiosk_settings as _kiosk_settings,
)
from ados.services.kiosk.config_watch import (
    _resolve_target_url as _resolve_target_url,
)
from ados.services.kiosk.config_watch import (
    _SettingsWatch as _SettingsWatch,
)
from ados.services.kiosk.config_watch import (
    _stop_on_settings_change as _stop_on_settings_change,
)
from ados.services.kiosk.desktop_session import (
    _DISPLAY_MANAGERS as _DISPLAY_MANAGERS,
)
from ados.services.kiosk.desktop_session import (
    _GRAPHICAL_SESSION_TYPES as _GRAPHICAL_SESSION_TYPES,
)
from ados.services.kiosk.desktop_session import (
    _SESSION_POLL_SECONDS as _SESSION_POLL_SECONDS,
)
from ados.services.kiosk.desktop_session import (
    _SESSION_WAIT_SECONDS as _SESSION_WAIT_SECONDS,
)
from ados.services.kiosk.desktop_session import (
    DesktopSession as DesktopSession,
)
from ados.services.kiosk.desktop_session import (
    _detect_desktop_session as _detect_desktop_session,
)
from ados.services.kiosk.desktop_session import (
    _detect_ready_session as _detect_ready_session,
)
from ados.services.kiosk.desktop_session import (
    _display_manager_active as _display_manager_active,
)
from ados.services.kiosk.desktop_session import (
    _loginctl_session_props as _loginctl_session_props,
)
from ados.services.kiosk.desktop_session import (
    _loginctl_sessions as _loginctl_sessions,
)
from ados.services.kiosk.desktop_session import (
    _resolve_desktop_session as _resolve_desktop_session,
)
from ados.services.kiosk.desktop_session import (
    _session_env as _session_env,
)
from ados.services.kiosk.desktop_session import (
    _session_socket_ready as _session_socket_ready,
)
from ados.services.kiosk.desktop_session import (
    _wayland_display_for as _wayland_display_for,
)
from ados.services.kiosk.desktop_session import (
    _wayland_sockets as _wayland_sockets,
)
from ados.services.kiosk.desktop_session import (
    _xauthority_for as _xauthority_for,
)
from ados.services.kiosk.display_probe import (
    _DISPLAY_POLL_SECONDS as _DISPLAY_POLL_SECONDS,
)
from ados.services.kiosk.display_probe import (
    _DRM_CARD_PATH as _DRM_CARD_PATH,
)
from ados.services.kiosk.display_probe import (
    _DRM_DEVICE as _DRM_DEVICE,
)
from ados.services.kiosk.display_probe import (
    _DRM_DIR as _DRM_DIR,
)
from ados.services.kiosk.display_probe import (
    _DRM_SYSFS as _DRM_SYSFS,
)
from ados.services.kiosk.display_probe import (
    _URL_POLL_SECONDS as _URL_POLL_SECONDS,
)
from ados.services.kiosk.display_probe import (
    _URL_PROBE_TIMEOUT as _URL_PROBE_TIMEOUT,
)
from ados.services.kiosk.display_probe import (
    _URL_WAIT_SECONDS as _URL_WAIT_SECONDS,
)
from ados.services.kiosk.display_probe import (
    _connected_outputs as _connected_outputs,
)
from ados.services.kiosk.display_probe import (
    _hdmi_present as _hdmi_present,
)
from ados.services.kiosk.display_probe import (
    _resolve_drm_device as _resolve_drm_device,
)
from ados.services.kiosk.display_probe import (
    _url_serving as _url_serving,
)
from ados.services.kiosk.display_probe import (
    _wait_for_display as _wait_for_display,
)
from ados.services.kiosk.display_probe import (
    _wait_for_url as _wait_for_url,
)
from ados.services.kiosk.display_probe import (
    hdmi_present as hdmi_present,
)
from ados.services.kiosk.factory import _make_supervisor as _make_supervisor
from ados.services.kiosk.launcher import (
    _BROWSER_CANDIDATES as _BROWSER_CANDIDATES,
)
from ados.services.kiosk.launcher import (
    _CAGE_RUNTIME_DIR as _CAGE_RUNTIME_DIR,
)
from ados.services.kiosk.launcher import (
    _CAGE_STORAGE_DIR as _CAGE_STORAGE_DIR,
)
from ados.services.kiosk.launcher import (
    _DISK_CACHE_BYTES as _DISK_CACHE_BYTES,
)
from ados.services.kiosk.launcher import (
    _ENV_RENDERER_KEY as _ENV_RENDERER_KEY,
)
from ados.services.kiosk.launcher import (
    _KIOSK_STORAGE_SUBDIR as _KIOSK_STORAGE_SUBDIR,
)
from ados.services.kiosk.launcher import (
    _RENDER_MARKER_PATH as _RENDER_MARKER_PATH,
)
from ados.services.kiosk.launcher import (
    _RENDERER_GPU as _RENDERER_GPU,
)
from ados.services.kiosk.launcher import (
    _RENDERER_SOFTWARE as _RENDERER_SOFTWARE,
)
from ados.services.kiosk.launcher import (
    _build_chromium_argv as _build_chromium_argv,
)
from ados.services.kiosk.launcher import (
    _build_windowed_chromium_argv as _build_windowed_chromium_argv,
)
from ados.services.kiosk.launcher import (
    _cage_env as _cage_env,
)
from ados.services.kiosk.launcher import (
    _chromium_render_flags as _chromium_render_flags,
)
from ados.services.kiosk.launcher import (
    _chromium_storage_flags as _chromium_storage_flags,
)
from ados.services.kiosk.launcher import (
    _normalise_renderer as _normalise_renderer,
)
from ados.services.kiosk.launcher import (
    _read_render_marker as _read_render_marker,
)
from ados.services.kiosk.launcher import (
    _resolve_browser_binary as _resolve_browser_binary,
)
from ados.services.kiosk.launcher import (
    _resolve_render_plan as _resolve_render_plan,
)
from ados.services.kiosk.supervisor import (
    _BROWSER_POLL_SECONDS as _BROWSER_POLL_SECONDS,
)
from ados.services.kiosk.supervisor import (
    _BROWSER_START_GRACE_SECONDS as _BROWSER_START_GRACE_SECONDS,
)
from ados.services.kiosk.supervisor import (
    _CRASH_LIMIT as _CRASH_LIMIT,
)
from ados.services.kiosk.supervisor import (
    _CRASH_WINDOW_SECONDS as _CRASH_WINDOW_SECONDS,
)
from ados.services.kiosk.supervisor import (
    _GPU_FAILURE_MARKERS as _GPU_FAILURE_MARKERS,
)
from ados.services.kiosk.supervisor import (
    _OUTPUT_MODE_POLL_SECONDS as _OUTPUT_MODE_POLL_SECONDS,
)
from ados.services.kiosk.supervisor import (
    _OUTPUT_MODE_WAIT_SECONDS as _OUTPUT_MODE_WAIT_SECONDS,
)
from ados.services.kiosk.supervisor import (
    _RETRY_SECONDS as _RETRY_SECONDS,
)
from ados.services.kiosk.supervisor import (
    _SHUTDOWN_GRACE_SECONDS as _SHUTDOWN_GRACE_SECONDS,
)
from ados.services.kiosk.supervisor import (
    _STDERR_LOG_LINE_LIMIT as _STDERR_LOG_LINE_LIMIT,
)
from ados.services.kiosk.supervisor import (
    _STDERR_TAIL_BYTES as _STDERR_TAIL_BYTES,
)
from ados.services.kiosk.supervisor import (
    KioskSupervisor as KioskSupervisor,
)
from ados.services.kiosk.supervisor import (
    _browser_running as _browser_running,
)
from ados.services.kiosk.supervisor import (
    _looks_gpu_failure as _looks_gpu_failure,
)


async def _amain() -> int:
    # Signature first: an edit that lands between it and the load is then seen
    # as a change by the watch below and re-read, never missed.
    signature = _config_signature()
    config = load_config()
    configure_logging(config.logging.level)
    slog = structlog.get_logger()
    slog.info("kiosk_service_starting")

    # The operator's display selection, honoured before anything touches the
    # panel. `lcd` and `none` both mean "this service does not own the screen",
    # and until now setting either did nothing at all — the value was written,
    # read by no one, and the kiosk started regardless.
    display = _display_selection(config)
    if display in ("lcd", "none"):
        slog.info(
            "kiosk_disabled_by_display_config",
            selection=display,
            msg=(
                "the panel is assigned elsewhere by ground_station.display.type; "
                "HDMI kiosk skipped cleanly"
            ),
        )
        return 0

    settings = _kiosk_settings(config)
    if not settings.enabled:
        slog.info(
            "kiosk_disabled_by_config",
            msg="ground_station.kiosk.enabled is false; HDMI kiosk stood down cleanly",
        )
        return 0
    watch = _SettingsWatch(settings, signature)

    if not await _wait_for_display(watch):
        slog.info(
            "kiosk_disabled_by_config",
            msg="kiosk disabled while waiting for a display; stood down cleanly",
        )
        return 0

    url, minimal = _resolve_target_url(config)
    # Wait for the cockpit URL to be served before launching the browser: on boot
    # the agent HTTP surface comes up after this service, and a browser pointed at
    # a not-yet-serving URL sticks on an error page (the operator sees "404 Not
    # Found" instead of the cockpit).
    await _wait_for_url(url)
    renderer, mali_lib_dir = _resolve_render_plan()
    # Adaptive launch: run a full-screen window inside a live desktop when one
    # is present (cage cannot — the desktop already owns the DRM master), else
    # own the display via cage. Waits briefly for a starting desktop when a
    # display manager is active (the boot race).
    session = await _resolve_desktop_session()
    slog.info(
        "kiosk_target_resolved",
        url=url,
        minimal_layer=minimal,
        renderer=renderer,
        gpu_lib_dir=mali_lib_dir,
        desktop_session=(session.session_type if session else None),
    )

    loop = asyncio.get_event_loop()
    # The active supervisor changes on a GPU->software downgrade; the signal
    # handler stops whichever one is current.
    current: dict[str, KioskSupervisor | None] = {"sup": None}

    def _on_signal(*_args: Any) -> None:
        slog.info("kiosk_service_signal_stop")
        sup = current["sup"]
        if sup is not None:
            sup.request_stop()

    for sig in (signal.SIGTERM, signal.SIGINT):
        try:
            loop.add_signal_handler(sig, _on_signal)
        except NotImplementedError:
            signal.signal(sig, _on_signal)

    # Up to one automatic GPU -> software downgrade if the GPU launch
    # crash-loops. Software (pixman / --disable-gpu) always renders, so a box
    # whose GPU userspace is wrong still ends up showing the cockpit.
    tried_software = renderer == _RENDERER_SOFTWARE
    while True:
        try:
            supervisor = _make_supervisor(
                url, session, renderer, mali_lib_dir, watch.settings.output_mode
            )
        except FileNotFoundError as exc:
            # No Chromium browser installed. Report which names were searched
            # and exit non-zero so the failure is visible without churning.
            slog.error("kiosk_binary_missing", error=str(exc))
            return 3
        current["sup"] = supervisor

        if session is not None:
            slog.info(
                "kiosk_desktop_session_detected",
                session_type=session.session_type,
                uid=session.uid,
                renderer=renderer,
            )
        else:
            slog.info(
                "kiosk_no_desktop_session",
                msg="owning the display via cage",
                renderer=renderer,
            )

        settings_watch = asyncio.create_task(
            _stop_on_settings_change(watch, supervisor), name="kiosk_settings_watch"
        )
        rc = await supervisor.run()
        settings_changed = settings_watch.done() and not settings_watch.cancelled()
        settings_watch.cancel()

        if settings_changed:
            if not watch.settings.enabled:
                slog.info(
                    "kiosk_disabled_by_config",
                    msg="ground_station.kiosk.enabled turned false; stood down cleanly",
                )
                return 0
            slog.info(
                "kiosk_restarting_for_settings",
                output_mode=watch.settings.output_mode,
            )
            continue

        # Self-heal: a crash-looping GPU cage launch downgrades to software so
        # the cockpit still ends up rendering (the windowed path is already
        # software, so this only applies when cage owns the display).
        if (
            supervisor.crash_looped
            and session is None
            and renderer == _RENDERER_GPU
            and not tried_software
        ):
            slog.error(
                "kiosk_gpu_fallback",
                msg="GPU renderer crash-looped; downgrading to software",
                stderr_tail=supervisor.last_stderr_tail,
                gpu_failure=_looks_gpu_failure(supervisor.last_stderr_tail),
            )
            renderer = _RENDERER_SOFTWARE
            mali_lib_dir = None
            tried_software = True
            continue

        slog.info("kiosk_service_stopped", rc=rc)
        return rc


def main() -> None:
    try:
        rc = asyncio.run(_amain())
    except KeyboardInterrupt:
        rc = 0
    sys.exit(rc)


if __name__ == "__main__":
    main()
