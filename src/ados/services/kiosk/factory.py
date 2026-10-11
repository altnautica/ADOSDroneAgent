"""HDMI kiosk: build the supervisor for a (session, renderer) combination."""

from __future__ import annotations

import os

from ados.services.kiosk._log import log
from ados.services.kiosk.desktop_session import DesktopSession, _session_env
from ados.services.kiosk.launcher import (
    _CAGE_RUNTIME_DIR,
    _CAGE_STORAGE_DIR,
    _KIOSK_STORAGE_SUBDIR,
    _build_chromium_argv,
    _build_windowed_chromium_argv,
    _cage_env,
)
from ados.services.kiosk.supervisor import KioskSupervisor


def _make_supervisor(
    url: str,
    session: DesktopSession | None,
    renderer: str,
    mali_lib_dir: str | None,
    output_mode: str | None = None,
) -> KioskSupervisor:
    """Build the supervisor for the current (session, renderer) combination.

    Windowed (a live desktop is present) attaches to that session's display
    server and honours the resolved ``renderer``, exactly as the cage path does.

    This deliberately does NOT force software any more. It used to, on the
    reasoning that a desktop owns its own GL stack which our scoped GPU
    userspace does not provision, so a desktop on a GPU-less board is on
    llvmpipe anyway. That holds for the boards the argument was written for — a
    Rockchip whose GL comes from our scoped libmali — but not for a board whose
    distro ships a real GL and video stack of its own. On a Raspberry Pi the
    desktop runs Mesa v3d with hardware decode on /dev/video10, and forcing
    ``--disable-gpu`` there threw all of it away: the ground station's HDMI
    cockpit software-decoded H.264 at ~113% CPU across four Chromium processes
    on four cores, which presents to the operator as a frozen picture while the
    stream underneath is perfectly healthy.

    Passing the resolved renderer is safe because the GPU choice is not final:
    a child that fails to bring up GL is downgraded to software by the
    supervisor's existing GPU-failure path, so a board that genuinely cannot
    drive a GPU still ends up rendering rather than crash-looping.

    cage (the appliance case) owns the display, with DISPLAY / WAYLAND_DISPLAY
    stripped and the renderer / DRM device / scoped libmali pinned. Raises
    ``FileNotFoundError`` when no Chromium is installed."""
    if session is not None:
        argv = _build_windowed_chromium_argv(
            url,
            session.session_type,
            renderer,
            # Under the session user's own runtime dir. logind creates it for
            # any active session, so a graphical session always has one, and it
            # is owned by the user the child is dropped to — which a dir under
            # the agent's root-owned runtime tree would not be.
            f"/run/user/{session.uid}/{_KIOSK_STORAGE_SUBDIR}",
        )
        # Run the browser AS the logged-in desktop user (not root): Chromium
        # refuses to run as root without --no-sandbox, and dropping to the user
        # keeps its sandbox and gives it a writable profile (HOME from
        # _session_env).
        return KioskSupervisor(
            argv,
            env=_session_env(session),
            sweep_orphans=False,
            run_as_uid=session.uid,
        )
    # Chromium will not start when the parent of --user-data-dir is absent, and
    # this one lives in a tmpfs that is empty on every boot, so create it here
    # rather than assuming. Running as root under cage, this is ours to make.
    try:
        os.makedirs(_CAGE_STORAGE_DIR, exist_ok=True)
    except OSError as exc:  # pragma: no cover - filesystem-dependent
        log.warning(
            "kiosk_storage_dir_unavailable",
            path=_CAGE_STORAGE_DIR,
            error=str(exc),
        )
    # cage refuses to bind its Wayland socket if XDG_RUNTIME_DIR is absent, and
    # the unit's /run/user/0 does not exist for a system service. 0700 because
    # the XDG spec requires it and libwayland complains otherwise.
    try:
        os.makedirs(_CAGE_RUNTIME_DIR, mode=0o700, exist_ok=True)
        os.chmod(_CAGE_RUNTIME_DIR, 0o700)
    except OSError as exc:  # pragma: no cover - filesystem-dependent
        log.warning(
            "kiosk_runtime_dir_unavailable",
            path=_CAGE_RUNTIME_DIR,
            error=str(exc),
        )
    argv = _build_chromium_argv(url, renderer)
    return KioskSupervisor(
        argv,
        env=_cage_env(renderer, mali_lib_dir),
        env_unset=frozenset({"DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY"}),
        output_mode=output_mode,
    )
