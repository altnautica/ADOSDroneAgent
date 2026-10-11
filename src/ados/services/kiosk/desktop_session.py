"""HDMI kiosk: detection of a live desktop session to launch a window into."""

from __future__ import annotations

import asyncio
import os
import pwd
import shutil
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path

from ados.services.kiosk._log import log

# ---------------------------------------------------------------------------
# Adaptive launch: run inside a live desktop when one is present, else own the
# display via cage.
# ---------------------------------------------------------------------------

# Session types loginctl reports for a graphical session.
_GRAPHICAL_SESSION_TYPES = ("wayland", "x11")


@dataclass(frozen=True)
class DesktopSession:
    """A running graphical login session the kiosk can launch a window into."""

    uid: int
    session_type: str  # "wayland" | "x11"
    display: str | None  # X11 DISPLAY (e.g. ":0"); None for wayland
    wayland_display: str | None  # wayland socket name; None for x11


def _loginctl_sessions() -> list[str]:
    """Return the session ids from ``loginctl``, or [] when loginctl is absent
    or fails (no systemd-logind → treat the box as having no managed desktop,
    so the kiosk owns the display via cage)."""
    loginctl = shutil.which("loginctl")
    if not loginctl:
        return []
    try:
        out = subprocess.run(
            [loginctl, "list-sessions", "--no-legend"],
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return []
    if out.returncode != 0:
        return []
    ids: list[str] = []
    for line in out.stdout.splitlines():
        parts = line.split()
        if parts:
            ids.append(parts[0])
    return ids


def _loginctl_session_props(session_id: str) -> dict[str, str]:
    """Return the ``key=value`` properties of one session, or {} on failure."""
    loginctl = shutil.which("loginctl")
    if not loginctl:
        return {}
    try:
        out = subprocess.run(
            [
                loginctl,
                "show-session",
                session_id,
                "-p",
                "Type",
                "-p",
                "State",
                "-p",
                "Active",
                "-p",
                "Remote",
                "-p",
                "User",
                "-p",
                "Display",
            ],
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return {}
    if out.returncode != 0:
        return {}
    props: dict[str, str] = {}
    for line in out.stdout.splitlines():
        key, _, value = line.partition("=")
        props[key.strip()] = value.strip()
    return props


def _wayland_sockets(runtime_dir: str) -> list[str]:
    """The Wayland socket names bound in ``runtime_dir``, sorted."""
    try:
        return sorted(
            n
            for n in os.listdir(runtime_dir)
            if n.startswith("wayland-") and not n.endswith(".lock")
        )
    except OSError:
        return []


def _wayland_display_for(uid: int) -> str:
    """Best-effort discovery of the wayland socket name in the user's runtime
    dir, defaulting to ``wayland-0`` (the common default) when none is found."""
    names = _wayland_sockets(f"/run/user/{uid}")
    return names[0] if names else "wayland-0"


def _xauthority_for(uid: int) -> str | None:
    """Locate the user's X authority cookie so an X11 launch can authenticate to
    the running X server. Best-effort across the common locations."""
    candidates: list[str] = []
    try:
        home = pwd.getpwuid(uid).pw_dir
        candidates.append(os.path.join(home, ".Xauthority"))
    except KeyError:
        pass
    candidates.append(f"/run/user/{uid}/.mutter-Xwaylandauth")
    candidates.append(f"/run/user/{uid}/gdm/Xauthority")
    for path in candidates:
        try:
            if os.path.exists(path):
                return path
        except OSError:
            continue
    return None


def _detect_desktop_session() -> DesktopSession | None:
    """Return the active graphical login session, or None when the box has no
    running desktop. A None result means the kiosk should own the display via
    cage; a session means it should launch a window into that desktop instead
    (cage cannot, because the desktop compositor already holds the DRM master)."""
    for session_id in _loginctl_sessions():
        props = _loginctl_session_props(session_id)
        stype = props.get("Type", "")
        if stype not in _GRAPHICAL_SESSION_TYPES:
            continue
        if props.get("Remote", "no") == "yes":
            continue
        if props.get("Active") != "yes" and props.get("State") != "active":
            continue
        try:
            uid = int(props.get("User", ""))
        except ValueError:
            continue
        if stype == "wayland":
            return DesktopSession(
                uid=uid,
                session_type="wayland",
                display=None,
                wayland_display=_wayland_display_for(uid),
            )
        return DesktopSession(
            uid=uid,
            session_type="x11",
            display=props.get("Display") or ":0",
            wayland_display=None,
        )
    return None


# Display managers whose presence means a desktop session is about to come up,
# so the kiosk should wait for it (and launch a window into it) instead of
# racing to grab the display via cage while the desktop is still starting.
_DISPLAY_MANAGERS = ("sddm", "gdm", "gdm3", "lightdm", "lxdm", "greetd")

# How long to wait for a starting desktop session before falling back to cage.
# Generous because the kiosk now starts at multi-user.target (before the
# desktop), so on a desktop box it waits here for the login session to become
# active; it returns as soon as the session appears, so a large ceiling is free.
_SESSION_WAIT_SECONDS = 90.0
_SESSION_POLL_SECONDS = 1.5


def _display_manager_active() -> bool:
    """True when a login/display manager unit is active — i.e. a desktop
    session is up or coming up. Used to decide whether to wait for the session
    before falling back to cage (avoids the boot race where the kiosk grabs the
    display via cage a moment before KDE/GNOME finishes starting)."""
    systemctl = shutil.which("systemctl")
    if not systemctl:
        return False
    for dm in _DISPLAY_MANAGERS:
        try:
            out = subprocess.run(
                [systemctl, "is-active", f"{dm}.service"],
                capture_output=True,
                text=True,
                timeout=5,
                check=False,
            )
        except (OSError, subprocess.SubprocessError):
            continue
        if out.stdout.strip() == "active":
            return True
    return False


def _session_socket_ready(session: DesktopSession) -> bool:
    """True when the session's display-server socket is actually bound, so a
    client can connect. A session can be 'active' in loginctl a moment before
    its Wayland/X socket exists — launching then fails with 'Failed to connect
    to Wayland display', which is exactly the first-attempt crash we want to
    avoid on boot."""
    if session.session_type == "wayland":
        sock = Path(f"/run/user/{session.uid}") / (session.wayland_display or "wayland-0")
        return sock.exists()
    # X11: DISPLAY ":N" -> /tmp/.X11-unix/XN.
    disp = (session.display or ":0").lstrip(":").split(".")[0]
    return Path(f"/tmp/.X11-unix/X{disp}").exists()


def _detect_ready_session() -> DesktopSession | None:
    """A detected desktop session whose display-server socket is up (ready for a
    client), else None so the caller keeps waiting."""
    session = _detect_desktop_session()
    if session is not None and _session_socket_ready(session):
        return session
    return None


async def _resolve_desktop_session() -> DesktopSession | None:
    """Return an active desktop session whose display-server socket is READY,
    waiting briefly for one when a display manager is active but the session /
    its socket has not come up yet (the boot race). Returns None on a genuinely
    headless / CLI box (no session and no display manager) so the caller owns
    the display via cage."""
    session = await asyncio.to_thread(_detect_ready_session)
    if session is not None:
        return session
    if not await asyncio.to_thread(_display_manager_active):
        return None
    log.info("kiosk_waiting_for_desktop_session", timeout_s=_SESSION_WAIT_SECONDS)
    deadline = time.monotonic() + _SESSION_WAIT_SECONDS
    while time.monotonic() < deadline:
        await asyncio.sleep(_SESSION_POLL_SECONDS)
        session = await asyncio.to_thread(_detect_ready_session)
        if session is not None:
            return session
    log.warning(
        "kiosk_desktop_session_timeout",
        msg="display manager active but no session became active; using cage",
    )
    return None


def _session_env(session: DesktopSession) -> dict[str, str]:
    """The environment overlay that lets a process launched by this service
    connect to the running desktop's display server. Also carries the session
    user's HOME/USER so the windowed browser — which runs AS that user, not
    root (Chromium refuses to run as root without --no-sandbox) — has a writable
    profile directory instead of inheriting the service's HOME=/root."""
    env: dict[str, str] = {"XDG_RUNTIME_DIR": f"/run/user/{session.uid}"}
    try:
        pw = pwd.getpwuid(session.uid)
        env["HOME"] = pw.pw_dir
        env["USER"] = pw.pw_name
        env["LOGNAME"] = pw.pw_name
    except KeyError:
        pass
    if session.session_type == "wayland":
        env["WAYLAND_DISPLAY"] = session.wayland_display or "wayland-0"
    else:
        env["DISPLAY"] = session.display or ":0"
        xauth = _xauthority_for(session.uid)
        if xauth:
            env["XAUTHORITY"] = xauth
    return env
