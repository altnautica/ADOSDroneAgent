"""HDMI kiosk service: Chromium full-screen, pointed at the agent cockpit.

HDMI + touch + gamepad should deliver a standalone field console with no
laptop required. This service owns the HDMI output. The default target is the
agent-served cockpit at ``http://localhost:8080/cockpit`` (a light SPA served
by the agent's own front, not a Next.js build on the box).

Lifecycle:
1. Probe `/dev/dri/card0`. If absent, the box has no HDMI sink connected
   (or the DRM driver did not bind). Log clearly and exit 0 so systemd
   does not churn restarting. Rule 26: the rest of the ground station
   keeps working even without HDMI.
2. Resolve target URL via config -> env var -> default chain.
3. Launch Chromium full-screen, adaptively:
   - When a graphical desktop session is already running on the box (a
     display manager with KDE / GNOME / etc.), launch Chromium as a
     full-screen kiosk window INSIDE that session. cage is NOT used here:
     it needs to own the DRM master, which the running desktop compositor
     already holds, so cage would fight the desktop and churn.
   - When no desktop is present (the appliance case), launch under `cage`,
     a Wayland single-app compositor that owns the display itself.
   The Chromium binary is resolved at runtime (its name varies by distro).
4. Supervise the child. On exit, backoff-restart. Five crashes in 60
   seconds flips to ERROR and we stop restarting so systemd can apply
   its own service-level retry.
5. On SIGTERM: send SIGTERM to the child, wait 10 s for graceful exit,
   SIGKILL if it is still up. Under cage we also sweep orphaned cage /
   chromium processes; inside a running desktop we do NOT broad-sweep
   chromium (that would kill the operator's own browser windows).

Not in scope:
- Bundling the cockpit here. It is served by the agent front at :8080.
- Sub-30 ms DRM-composited low-latency video (a v2 optimization).
"""

from __future__ import annotations

import asyncio
import os
import pwd
import shutil
import signal
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import structlog

from ados.core.config import load_config
from ados.core.logging import configure_logging, get_logger

log = get_logger("kiosk.kiosk_service")

_DRM_CARD_PATH = Path("/dev/dri/card0")
_DRM_DIR = Path("/dev/dri")
_DRM_SYSFS = Path("/sys/class/drm")

# The DRM display devices can appear a few seconds AFTER multi-user/graphical
# is reached at boot (the GPU/KMS driver probes asynchronously), so a one-shot
# presence check loses a boot race and the kiosk never starts. We wait for a
# display to appear instead of gating on it once.
_DISPLAY_WAIT_SECONDS = 60.0
_DISPLAY_POLL_SECONDS = 2.0

_DEFAULT_URL = "http://localhost:8080/cockpit"
_ENV_URL_KEY = "ADOS_KIOSK_URL"
_ENV_MINIMAL_KEY = "ADOS_KIOSK_MINIMAL_LAYER"
_ENV_RENDERER_KEY = "ADOS_KIOSK_RENDERER"

# Renderer selection.
#
# "software" (pixman / CPU) is the safe default: it never touches the GPU and
# renders on ANY board, so a fresh box always shows the cockpit. "gpu" (cage
# WLR_RENDERER=gles2 + Chromium EGL) is opt-in, enabled only when the installer
# has provisioned a working GPU userspace (e.g. the Rockchip libmali blob for a
# Mali board) and recorded it in the render marker below.
#
# We deliberately do NOT run a live EGL probe here to auto-detect the GPU: on
# some Rockchip boards, poking the GPU through a mismatched driver stack (the
# stock Mesa libEGL against a Valhall-CSF Mali) hangs the whole box, and the
# kiosk runs on every boot. The installer decides once, from the HAL, and writes
# the marker; the kiosk trusts it, with a self-healing downgrade to software if
# a GPU-mode child still crash-loops.
_RENDER_MARKER_PATH = Path("/etc/ados/kiosk-render.conf")
_RENDERER_GPU = "gpu"
_RENDERER_SOFTWARE = "software"

# The DRM device cage should own in the appliance (no-desktop) case.
_DRM_DEVICE = "/dev/dri/card0"

# Substrings in a child's stderr that mark a GPU/EGL/renderer init failure,
# used to decide whether a GPU-mode crash should downgrade to software.
_GPU_FAILURE_MARKERS = (
    "failed to create renderer",
    "failed to load driver",
    "eglinitialize",
    "egl_not_initialized",
    "dri2",
    "gbm",
    "could not match drm and vulkan",
    "no drm fd",
    "wlr_renderer",
)

# Crash-loop guard.
_CRASH_WINDOW_SECONDS = 60.0
_CRASH_LIMIT = 5
_BACKOFF_START_SECONDS = 3.0
_BACKOFF_MAX_SECONDS = 30.0

# Graceful shutdown allowance for the cage child.
_SHUTDOWN_GRACE_SECONDS = 10.0

# Minimal-layer auto-trigger threshold. Boards under 3 GiB default to the
# reduced render path so Chromium stays within its memory envelope.
_MINIMAL_RAM_THRESHOLD_BYTES = 3 * 1024 * 1024 * 1024

_STDERR_TAIL_BYTES = 2048

# Chromium browser binary candidates, in resolution order. The binary name
# varies by distro: Raspberry Pi OS historically shipped `chromium-browser`;
# Debian, Armbian, and current Raspberry Pi OS ship `chromium`
# (`/usr/bin/chromium`). `-stable` is the flatpak/snap-adjacent name some
# images expose. The installer installs whichever apt package is available; the
# kiosk resolves the binary at runtime so it does not depend on one fixed name.
_BROWSER_CANDIDATES = ("chromium-browser", "chromium", "chromium-browser-stable")


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


async def _wait_for_display() -> bool:
    """Wait (bounded) for a DRM display to appear, absorbing the boot race where
    the KMS device is created shortly after the service starts. Returns True as
    soon as one is present, False after the timeout (a genuinely headless box)."""
    if _hdmi_present():
        return True
    log.info("kiosk_waiting_for_display", timeout_s=_DISPLAY_WAIT_SECONDS)
    deadline = time.monotonic() + _DISPLAY_WAIT_SECONDS
    while time.monotonic() < deadline:
        await asyncio.sleep(_DISPLAY_POLL_SECONDS)
        if _hdmi_present():
            return True
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


def _low_ram_board() -> bool:
    try:
        import psutil
    except Exception:
        return False
    try:
        total = psutil.virtual_memory().total
    except Exception:
        return False
    return total < _MINIMAL_RAM_THRESHOLD_BYTES


def _resolve_target_url(config: Any) -> tuple[str, bool]:
    """Config -> env -> default. Returns (url_with_query, minimal_flag).

    Query string `?layer=minimal` is appended when either the config
    flag is true or the board has less than 3 GiB RAM. An explicit
    env override `ADOS_KIOSK_MINIMAL_LAYER=0` forces the full layer.
    """
    cfg_url, cfg_minimal = _get_kiosk_config(config)

    url = cfg_url or os.environ.get(_ENV_URL_KEY) or _DEFAULT_URL

    minimal = False
    if cfg_minimal is True:
        minimal = True
    elif _low_ram_board():
        minimal = True

    env_minimal = os.environ.get(_ENV_MINIMAL_KEY)
    if env_minimal is not None:
        minimal = env_minimal.strip() not in ("0", "false", "False", "")

    if minimal:
        sep = "&" if "?" in url else "?"
        url = f"{url}{sep}layer=minimal"

    return url, minimal


def _normalise_renderer(value: str) -> str | None:
    v = value.strip().lower()
    if v in (_RENDERER_GPU, "gles2", "gles", "egl"):
        return _RENDERER_GPU
    if v in (_RENDERER_SOFTWARE, "pixman", "sw", "cpu"):
        return _RENDERER_SOFTWARE
    return None


def _read_render_marker() -> tuple[str | None, str | None]:
    """Return ``(renderer, mali_lib_dir)`` from the install-written marker.

    The marker is a tiny ``key: value`` file the installer writes only after it
    provisioned a working GPU userspace for this board::

        renderer: gpu
        lib_dir: /opt/ados/gpu/mali

    ``renderer`` is "gpu" or "software"; ``lib_dir`` (optional) is the private
    directory holding the SCOPED libmali EGL/GLES/GBM so cage can use the GPU
    without the system's Mesa libEGL being replaced. Absent / unreadable ->
    ``(None, None)`` (caller defaults to software).
    """
    try:
        text = _RENDER_MARKER_PATH.read_text(encoding="utf-8")
    except OSError:
        return None, None
    renderer: str | None = None
    lib_dir: str | None = None
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        # Accept "renderer: gpu" or "renderer = gpu".
        key, sep, value = line.partition(":")
        if not sep:
            key, sep, value = line.partition("=")
        key = key.strip().lower()
        value = value.strip()
        if key == "renderer":
            renderer = _normalise_renderer(value)
        elif key == "lib_dir" and value:
            lib_dir = value
    return renderer, lib_dir


def _resolve_render_plan() -> tuple[str, str | None]:
    """Return ``(renderer, mali_lib_dir)`` for the cage launch.

    Precedence: the ``ADOS_KIOSK_RENDERER`` env override -> the install-written
    marker -> ``"software"`` (the safe default that always renders). A marker
    "gpu" is trusted only when its scoped libmali directory still exists (a
    stale marker whose libs were removed falls back to software cleanly). See
    ``_RENDER_MARKER_PATH`` for why there is no live GPU probe here.
    """
    env = os.environ.get(_ENV_RENDERER_KEY)
    if env:
        chosen = _normalise_renderer(env)
        if chosen == _RENDERER_SOFTWARE:
            return _RENDERER_SOFTWARE, None
        if chosen == _RENDERER_GPU:
            # Operator override wins; still carry the marker's scoped lib dir.
            _, lib_dir = _read_render_marker()
            return _RENDERER_GPU, lib_dir
    renderer, lib_dir = _read_render_marker()
    if renderer == _RENDERER_GPU:
        if lib_dir and not Path(lib_dir).is_dir():
            return _RENDERER_SOFTWARE, None
        return _RENDERER_GPU, lib_dir
    return _RENDERER_SOFTWARE, None


def _cage_env(renderer: str, mali_lib_dir: str | None) -> dict[str, str]:
    """Environment overlay for the cage (appliance) launch.

    Pins cage's renderer and DRM device. ``DISPLAY`` / ``WAYLAND_DISPLAY`` are
    stripped separately (via ``env_unset``) so cage uses its own DRM backend
    instead of trying to nest under an X11 / Wayland server that is not there.
    When the GPU renderer is active and a scoped libmali directory is
    provisioned, it is prepended to ``LD_LIBRARY_PATH`` so cage + Chromium load
    the GPU EGL/GLES/GBM from there, WITHOUT the system Mesa libEGL being
    touched (so a running desktop and the software fallback are never broken).
    """
    wlr = "gles2" if renderer == _RENDERER_GPU else "pixman"
    env = {
        "WLR_RENDERER": wlr,
        "WLR_DRM_DEVICES": _DRM_DEVICE,
    }
    if renderer == _RENDERER_GPU and mali_lib_dir:
        # Scope libmali to this process tree only (cage + Chromium), so the GPU
        # EGL/GLES/GBM shadow Mesa WITHOUT the system libEGL being replaced. We do
        # NOT set EGL_PLATFORM: cage selects GBM and Chromium selects Wayland
        # explicitly, and forcing a single platform here would break the client
        # that wanted the other one.
        existing = os.environ.get("LD_LIBRARY_PATH", "")
        env["LD_LIBRARY_PATH"] = (
            f"{mali_lib_dir}:{existing}" if existing else mali_lib_dir
        )
    elif renderer == _RENDERER_SOFTWARE:
        # No GPU cursor plane in the software path; a hardware cursor on a
        # pixman renderer is a known wlroots crash on some SBCs.
        env["WLR_NO_HARDWARE_CURSORS"] = "1"
    return env


def _resolve_browser_binary() -> str:
    """Return the first Chromium browser binary present on PATH.

    The binary name varies by distro (see `_BROWSER_CANDIDATES`). Probe the
    known names in order and return the absolute path of the first that
    resolves. Raise `FileNotFoundError` naming every tried candidate when none
    is present, so the supervisor's `kiosk_binary_missing` path reports exactly
    what was searched instead of a bare `chromium-browser` not-found.
    """
    for name in _BROWSER_CANDIDATES:
        found = shutil.which(name)
        if found:
            return found
    raise FileNotFoundError(
        "no Chromium browser binary found on PATH; tried: "
        + ", ".join(_BROWSER_CANDIDATES)
    )


def _chromium_render_flags(renderer: str) -> list[str]:
    """Chromium flags for the chosen renderer.

    GPU: Wayland + EGL + GPU rasterization (hardware accelerated, used only
    when the GPU userspace is provisioned). Software: ``--disable-gpu`` so
    Chromium composites on the CPU and never opens the GPU EGL, matching cage's
    pixman renderer so nothing in the stack touches a GPU that cannot be driven.
    """
    if renderer == _RENDERER_GPU:
        return ["--use-gl=egl", "--enable-gpu-rasterization"]
    return ["--disable-gpu"]


def _build_chromium_argv(url: str, renderer: str) -> list[str]:
    """Full argv for `cage -- <chromium> ...`.

    The browser binary is resolved at runtime (`_resolve_browser_binary`)
    because its package/binary name varies by distro. cage handles the Wayland
    compositor; the GPU-vs-software Chromium flags follow ``renderer`` (matched
    to cage's WLR_RENDERER). Raises `FileNotFoundError` (propagated to the
    `kiosk_binary_missing` path) when no browser is installed.
    """
    browser = _resolve_browser_binary()
    return [
        "cage",
        "--",
        browser,
        "--kiosk",
        "--noerrdialogs",
        "--disable-infobars",
        "--no-first-run",
        # cage owns the display as root, so Chromium runs as root here and
        # refuses to start without --no-sandbox. (The windowed path avoids this
        # by running Chromium as the logged-in desktop user, keeping its sandbox.)
        "--no-sandbox",
        "--ozone-platform=wayland",
        *_chromium_render_flags(renderer),
        "--autoplay-policy=no-user-gesture-required",
        url,
    ]


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


def _wayland_display_for(uid: int) -> str:
    """Best-effort discovery of the wayland socket name in the user's runtime
    dir, defaulting to ``wayland-0`` (the common default) when none is found."""
    runtime_dir = f"/run/user/{uid}"
    try:
        names = sorted(
            n
            for n in os.listdir(runtime_dir)
            if n.startswith("wayland-") and not n.endswith(".lock")
        )
    except OSError:
        names = []
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
    session = _detect_ready_session()
    if session is not None:
        return session
    if not _display_manager_active():
        return None
    log.info("kiosk_waiting_for_desktop_session", timeout_s=_SESSION_WAIT_SECONDS)
    deadline = time.monotonic() + _SESSION_WAIT_SECONDS
    while time.monotonic() < deadline:
        await asyncio.sleep(_SESSION_POLL_SECONDS)
        session = _detect_ready_session()
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


def _build_windowed_chromium_argv(
    url: str, session_type: str, renderer: str
) -> list[str]:
    """Full argv for a full-screen Chromium kiosk WITHOUT cage, to run inside an
    already-running desktop session. The Ozone platform matches the session so
    Chromium attaches to the live compositor / X server rather than trying to
    own the display. The GPU-vs-software flags follow ``renderer`` (a desktop on
    a board with no GPU userspace runs on llvmpipe, where ``--disable-gpu`` is
    the reliable path). Raises ``FileNotFoundError`` (propagated to the
    ``kiosk_binary_missing`` path) when no browser is installed."""
    browser = _resolve_browser_binary()
    platform = "wayland" if session_type == "wayland" else "x11"
    return [
        browser,
        "--kiosk",
        "--start-fullscreen",
        "--noerrdialogs",
        "--disable-infobars",
        "--no-first-run",
        f"--ozone-platform={platform}",
        *_chromium_render_flags(renderer),
        "--autoplay-policy=no-user-gesture-required",
        url,
    ]


class KioskSupervisor:
    """Spawn and supervise the cage + Chromium child process."""

    def __init__(
        self,
        argv: list[str],
        *,
        env: dict[str, str] | None = None,
        env_unset: frozenset[str] | set[str] | None = None,
        sweep_orphans: bool = True,
        run_as_uid: int | None = None,
    ) -> None:
        self._argv = argv
        # When set, the child is dropped to this uid (and its primary gid) before
        # exec — the windowed path runs Chromium as the logged-in desktop user
        # rather than root, because Chromium refuses to run as root without
        # --no-sandbox. None (the cage path) runs as the service user (root).
        self._run_as_uid = run_as_uid
        # An environment overlay merged over the service env (used to attach a
        # windowed launch to a running desktop's display server, or to pin
        # cage's renderer/DRM device). None inherits the service env unchanged.
        self._env = env
        # Keys to REMOVE from the child env after the overlay merge. The cage
        # path strips DISPLAY / WAYLAND_DISPLAY so cage uses its own DRM backend
        # instead of trying (and failing) to nest under an absent X11/Wayland
        # server — the root of the historical "Failed to open xcb connection".
        self._env_unset = env_unset
        # Whether to broad-pkill cage/chromium orphans on stop. True under cage
        # (safe — cage owns the only chromium). False inside a running desktop,
        # where a broad chromium sweep would kill the operator's own browser.
        self._sweep_orphans_enabled = sweep_orphans
        self._proc: asyncio.subprocess.Process | None = None
        self._stop = asyncio.Event()
        self._crash_times: list[float] = []
        # Set True when the crash-loop guard trips (5 crashes / 60 s). Lets the
        # caller downgrade a crash-looping GPU launch to the software renderer.
        self.crash_looped = False
        # Last child's stderr tail, for the caller's downgrade heuristic.
        self.last_stderr_tail = ""

    def request_stop(self) -> None:
        self._stop.set()

    async def _spawn(self) -> asyncio.subprocess.Process:
        log.info("kiosk_spawning", argv=self._argv, run_as_uid=self._run_as_uid)
        spawn_env: dict[str, str] | None
        if self._env is None and not self._env_unset:
            spawn_env = None
        else:
            spawn_env = {**os.environ, **(self._env or {})}
            for key in self._env_unset or ():
                spawn_env.pop(key, None)
        # Drop to the desktop user for the windowed path (Chromium refuses root).
        # `user`/`group` setgid+setuid before exec (Python 3.9+). We resolve the
        # primary gid ourselves so supplementary groups are dropped too.
        extra: dict[str, Any] = {}
        if self._run_as_uid is not None:
            extra["user"] = self._run_as_uid
            try:
                extra["group"] = pwd.getpwuid(self._run_as_uid).pw_gid
            except KeyError:
                pass
        return await asyncio.create_subprocess_exec(
            *self._argv,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=spawn_env,
            # Own session so a windowed Chromium's whole tree is killable via
            # the child, without a broad pkill that would hit the desktop's
            # other browsers.
            start_new_session=True,
            **extra,
        )

    async def _graceful_kill(self, proc: asyncio.subprocess.Process) -> None:
        if proc.returncode is None:
            try:
                proc.terminate()
            except ProcessLookupError:
                pass
            else:
                try:
                    await asyncio.wait_for(
                        proc.wait(), timeout=_SHUTDOWN_GRACE_SECONDS
                    )
                    log.info("kiosk_child_terminated", rc=proc.returncode)
                except TimeoutError:
                    log.warning("kiosk_child_sigterm_timeout", pid=proc.pid)
                    try:
                        proc.kill()
                        await proc.wait()
                        log.warning("kiosk_child_killed", rc=proc.returncode)
                    except ProcessLookupError:
                        pass

        # cage may leave an orphaned chromium-browser process when it is
        # torn down under load. Sweep both names best-effort so systemd
        # sees a clean exit. Idempotent: pkill returns non-zero when
        # nothing matched, which is fine. Skipped inside a running desktop,
        # where a broad chromium pkill would also kill the operator's own
        # browser windows — there, terminating our own child (a Chromium that
        # shuts its tree down on SIGTERM) is enough.
        if self._sweep_orphans_enabled:
            await self._sweep_orphans()

    async def _sweep_orphans(self) -> None:
        """Best-effort pkill sweep of cage and chromium-browser children."""
        for name, first_sig in (("cage", "-TERM"), ("chromium", "-TERM")):
            await self._run_pkill(first_sig, name)
            await asyncio.sleep(1.0)
            await self._run_pkill("-KILL", name)

    @staticmethod
    async def _run_pkill(sig: str, name: str) -> None:
        try:
            proc = await asyncio.create_subprocess_exec(
                "pkill",
                sig,
                "-f",
                name,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            try:
                await asyncio.wait_for(proc.wait(), timeout=3.0)
            except TimeoutError:
                try:
                    proc.kill()
                except ProcessLookupError:
                    pass
        except (FileNotFoundError, OSError) as exc:
            log.debug("kiosk_pkill_skipped", sig=sig, name=name, error=str(exc))

    def _record_crash_and_check(self) -> bool:
        """Append now() to crash log, prune outside window. Return True if under limit."""
        now = time.monotonic()
        self._crash_times.append(now)
        self._crash_times = [
            t for t in self._crash_times if (now - t) <= _CRASH_WINDOW_SECONDS
        ]
        return len(self._crash_times) < _CRASH_LIMIT

    @staticmethod
    def _tail_bytes(data: bytes, limit: int = _STDERR_TAIL_BYTES) -> str:
        if not data:
            return ""
        trimmed = data[-limit:]
        try:
            return trimmed.decode("utf-8", errors="replace").strip()
        except Exception:
            return ""

    async def run(self) -> int:
        """Supervise loop. Returns process exit code or 0 on clean stop."""
        backoff = _BACKOFF_START_SECONDS
        while not self._stop.is_set():
            try:
                self._proc = await self._spawn()
            except FileNotFoundError as exc:
                log.error("kiosk_binary_missing", error=str(exc))
                return 3
            except Exception as exc:
                log.error("kiosk_spawn_failed", error=str(exc))
                return 4

            proc = self._proc
            log.info("kiosk_child_running", pid=proc.pid)
            backoff = _BACKOFF_START_SECONDS

            wait_task = asyncio.create_task(proc.wait(), name="kiosk_child_wait")
            stop_task = asyncio.create_task(self._stop.wait(), name="kiosk_stop_wait")
            done, pending = await asyncio.wait(
                {wait_task, stop_task}, return_when=asyncio.FIRST_COMPLETED
            )

            if stop_task in done:
                for t in pending:
                    t.cancel()
                await self._graceful_kill(proc)
                log.info("kiosk_supervisor_stopping")
                return 0

            # Child exited on its own.
            for t in pending:
                t.cancel()

            rc = proc.returncode if proc.returncode is not None else -1
            stderr_data = b""
            try:
                if proc.stderr is not None:
                    stderr_data = await asyncio.wait_for(proc.stderr.read(), timeout=1.0)
            except Exception:
                pass
            stderr_tail = self._tail_bytes(stderr_data)
            self.last_stderr_tail = stderr_tail

            under_limit = self._record_crash_and_check()
            log.warning(
                "kiosk_child_exited",
                rc=rc,
                stderr_tail=stderr_tail,
                crashes_in_window=len(self._crash_times),
            )

            if not under_limit:
                self.crash_looped = True
                log.error(
                    "kiosk_crash_loop_guard",
                    msg="5 crashes in 60s, stopping restart loop",
                    last_rc=rc,
                )
                return rc if rc >= 0 else 5

            # Exponential backoff, capped.
            try:
                await asyncio.wait_for(self._stop.wait(), timeout=backoff)
                # If stop fires during backoff, exit cleanly.
                return 0
            except TimeoutError:
                pass
            backoff = min(backoff * 2, _BACKOFF_MAX_SECONDS)

        return 0


def _looks_gpu_failure(stderr_tail: str) -> bool:
    """True when a child's stderr tail carries a GPU/EGL/renderer-init failure
    marker — a diagnostic hint for the GPU->software downgrade (the downgrade
    itself does not depend on it)."""
    low = stderr_tail.lower()
    return any(marker in low for marker in _GPU_FAILURE_MARKERS)


def _make_supervisor(
    url: str,
    session: DesktopSession | None,
    renderer: str,
    mali_lib_dir: str | None,
) -> KioskSupervisor:
    """Build the supervisor for the current (session, renderer) combination.

    Windowed (a live desktop is present) attaches to that session's display
    server and renders in SOFTWARE: a desktop owns its own GL stack and our
    scoped GPU userspace does not touch it, so ``--disable-gpu`` is the safe
    match (a desktop on a board with no GPU userspace is on llvmpipe anyway).
    cage (the appliance case) owns the display, with DISPLAY / WAYLAND_DISPLAY
    stripped and the renderer / DRM device / scoped libmali pinned. Raises
    ``FileNotFoundError`` when no Chromium is installed."""
    if session is not None:
        argv = _build_windowed_chromium_argv(
            url, session.session_type, _RENDERER_SOFTWARE
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
    argv = _build_chromium_argv(url, renderer)
    return KioskSupervisor(
        argv,
        env=_cage_env(renderer, mali_lib_dir),
        env_unset=frozenset({"DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY"}),
    )


async def _amain() -> int:
    config = load_config()
    configure_logging(config.logging.level)
    slog = structlog.get_logger()
    slog.info("kiosk_service_starting")

    if not await _wait_for_display():
        slog.info(
            "kiosk_hdmi_absent",
            msg="no DRM display after wait; HDMI kiosk skipped cleanly",
        )
        return 0

    url, minimal = _resolve_target_url(config)
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
            supervisor = _make_supervisor(url, session, renderer, mali_lib_dir)
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

        rc = await supervisor.run()

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
