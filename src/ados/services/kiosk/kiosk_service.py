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
from ados.core.paths import ADOS_RUN_DIR

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

# The agent HTTP surface that serves the cockpit (the native control front and
# the FastAPI app it proxies) finishes starting a few seconds after this service
# on boot. A browser launched before the URL is served loads an error page and
# STICKS there (no auto-retry), so the operator sees a "404 Not Found" instead of
# the cockpit. We poll the target URL until it is served before launching.
_URL_WAIT_SECONDS = 90.0
_URL_POLL_SECONDS = 1.0
_URL_PROBE_TIMEOUT = 3.0

# Trailing slash on purpose: it is the path the static mount actually serves,
# so the kiosk never pays a redirect on boot and the query string this module
# appends is not at the mercy of one.
_DEFAULT_URL = "http://localhost:8080/cockpit/"
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

# Fallback DRM device for the appliance (no-desktop) case when the connected
# card cannot be determined. Prefer `_resolve_drm_device()`, which finds the
# card that actually drives a display — `_hdmi_present` already knows these can
# differ, and handing cage the wrong one leaves it with no connector to modeset.
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

# How many child stderr lines are forwarded to the journal before the rest are
# suppressed. A browser is chatty; the point is to make a failure visible, not
# to relay every frame's noise onto a flash-backed journal.
_STDERR_LOG_LINE_LIMIT = 40

# The browser is spawned BY the compositor, so it is legitimately absent for a
# moment after launch. Wait this long before concluding it is missing.
_BROWSER_START_GRACE_SECONDS = 25.0
_BROWSER_POLL_SECONDS = 10.0

# Chromium browser binary candidates, in resolution order. The binary name
# varies by distro: Raspberry Pi OS historically shipped `chromium-browser`;
# Debian, Armbian, and current Raspberry Pi OS ship `chromium`
# (`/usr/bin/chromium`). `-stable` is the flatpak/snap-adjacent name some
# images expose. The installer installs whichever apt package is available; the
# kiosk resolves the binary at runtime so it does not depend on one fixed name.
_BROWSER_CANDIDATES = ("chromium-browser", "chromium", "chromium-browser-stable")

# Directory name for the kiosk browser's profile and cache inside a session
# user's runtime dir (the windowed path). See _chromium_storage_flags.
_KIOSK_STORAGE_SUBDIR = "ados-kiosk"

# Upper bound on the browser disk cache. It lives in tmpfs, so this is a RAM
# budget, not a disk one — generous for a single local page, small enough that
# it can never compete with the video pipeline for memory.
_DISK_CACHE_BYTES = 64 * 1024 * 1024

# Where the appliance (cage) launch keeps browser storage. The agent's own
# runtime dir, which is tmpfs and which the agent creates and owns, so it is
# guaranteed to exist and to be writable by the root-run cage child.
#
# Deliberately NOT $XDG_RUNTIME_DIR (/run/user/0): systemd does not create a
# runtime dir for a system service, so that path can simply be absent, and
# Chromium given a --user-data-dir whose parent does not exist fails to start.
_CAGE_STORAGE_DIR = str(ADOS_RUN_DIR / "kiosk")


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
        "WLR_DRM_DEVICES": _resolve_drm_device(),
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

    GPU: let Chromium pick its own GL implementation. **Do not name one.**

    This used to pass ``--use-gl=egl``, and that flag has since been renamed
    upstream to ``--gl=``. On a current Chromium the old spelling is not
    rejected loudly — it resolves to "no implementation", so the GPU process
    exits during initialization, over and over, and the panel stays black while
    every other health signal reads fine. Measured on a ground station running
    Chromium 150::

        (no flag)               gpu_process_exits=0
        --gl=egl-angle          gpu_process_exits=0
        --use-angle=gl          gpu_process_exits=0
        --use-gl=egl            gpu_process_exits=4   <- the only failing option

    Several spellings work; passing none of them also works, and is the only
    option that cannot go stale the same way. Chromium's default on Linux is
    already the ANGLE/EGL path we were trying to ask for, so naming it bought
    nothing and cost a black screen.

    ``--enable-gpu-rasterization`` is likewise dropped: it has been the default
    for years, and carrying a flag whose behaviour is now the default is how the
    previous one survived long enough to break.

    Software: ``--disable-gpu`` so Chromium composites on the CPU and never
    opens the GPU EGL, matching cage's pixman renderer so nothing in the stack
    touches a GPU that cannot be driven.
    """
    if renderer == _RENDERER_GPU:
        return []
    return ["--disable-gpu"]


def _chromium_storage_flags(base_dir: str) -> list[str]:
    """Pin Chromium's profile and cache into the runtime tmpfs, with a bounded
    cache.

    Left to itself Chromium writes its HTTP cache, code cache, shader cache,
    Local Storage, cookies and history under ``$HOME``. Under cage the service
    runs as root, so that is ``/root/.cache/chromium`` and ``/root/.config/
    chromium`` — on the SD card, unbounded, and rewritten continuously because
    the page it is showing is a live-updating SPA with a video stream. A ground
    station runs that page for its whole life.

    None of it is worth persisting. The kiosk shows one page, served from
    localhost, with no login and no session to carry across a reboot; the cache
    exists to avoid a network round trip that is not happening anyway. So it all
    goes to a tmpfs directory and dies with the boot.

    The size cap matters *because* the target is tmpfs: an unbounded cache there
    would trade SD wear for RAM exhaustion, which on a 4 GB board sharing memory
    with a video pipeline is not a trade worth making.

    Applying this on the windowed path too has a second, unrelated benefit: the
    kiosk stops sharing a profile directory with the desktop user's own browser,
    so launching it can no longer collide with a Chromium the operator already
    has open.
    """
    base = base_dir.rstrip("/")
    return [
        f"--user-data-dir={base}/profile",
        f"--disk-cache-dir={base}/cache",
        f"--disk-cache-size={_DISK_CACHE_BYTES}",
    ]


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
        *_chromium_storage_flags(_CAGE_STORAGE_DIR),
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
    url: str, session_type: str, renderer: str, storage_dir: str
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
        *_chromium_storage_flags(storage_dir),
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

    async def _stream_stderr(self, proc: Any) -> None:
        """Forward the child's stderr to the journal line by line, and keep the
        rolling tail the GPU-downgrade heuristic reads.

        Bounded on purpose: a browser can be extremely chatty, and the point is
        to make a failure visible, not to relay every frame's worth of noise
        onto a flash-backed journal.
        """
        if proc.stderr is None:
            return
        seen = 0
        recent: list[str] = []
        try:
            while True:
                raw = await proc.stderr.readline()
                if not raw:
                    break
                line = raw.decode("utf-8", errors="replace").rstrip()
                if not line:
                    continue
                recent.append(line)
                if len(recent) > 40:
                    recent.pop(0)
                self.last_stderr_tail = "\n".join(recent)[-_STDERR_TAIL_BYTES:]
                if seen < _STDERR_LOG_LINE_LIMIT:
                    log.warning("kiosk_child_stderr", line=line[:400])
                elif seen == _STDERR_LOG_LINE_LIMIT:
                    log.warning(
                        "kiosk_child_stderr_suppressed",
                        msg=f"further child stderr suppressed after {seen} lines",
                    )
                seen += 1
        except asyncio.CancelledError:
            raise
        except Exception:
            return

    async def _watch_browser(self, proc: Any) -> None:
        """Resolve once the BROWSER is gone while the compositor is still up.

        Only meaningful on the cage path, where the supervisor's child is the
        compositor and the browser is its grandchild. On the windowed path the
        child IS the browser, so `proc.wait()` already covers it and this never
        fires.

        The browser is given a grace period to appear: it is spawned by cage, so
        it is legitimately absent for a moment right after launch.
        """
        if not self._sweep_orphans_enabled:
            return  # windowed path: the child is the browser itself
        await asyncio.sleep(_BROWSER_START_GRACE_SECONDS)
        while True:
            if proc.returncode is not None:
                return  # the compositor went first; the normal path handles it
            if not _browser_running():
                return
            await asyncio.sleep(_BROWSER_POLL_SECONDS)

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
            # Drain the child's stderr WHILE it runs, not only when it exits.
            #
            # A compositor that stays up with a broken browser inside it never
            # exits, so an exit-time read never happens and the error is never
            # seen. That is exactly how a black panel presented as a healthy
            # unit: the last journal line was `kiosk_child_running`, and the GPU
            # initialization error that caused it was only visible by running
            # the argv by hand.
            drain_task = asyncio.create_task(
                self._stream_stderr(proc), name="kiosk_child_stderr"
            )
            # And watch the BROWSER, not just the compositor we launched. The
            # supervisor's child is `cage`; the browser is its grandchild, so a
            # dead browser under a live cage is invisible to `proc.wait()`.
            browser_task = asyncio.create_task(
                self._watch_browser(proc), name="kiosk_browser_watch"
            )
            done, pending = await asyncio.wait(
                {wait_task, stop_task, browser_task},
                return_when=asyncio.FIRST_COMPLETED,
            )
            if browser_task in done and wait_task not in done and stop_task not in done:
                # The compositor is still alive but has nothing to show. Treat it
                # as a crash so the existing backoff + GPU-downgrade machinery
                # handles it, rather than leaving a black screen reading healthy.
                log.error(
                    "kiosk_browser_vanished",
                    msg="the browser exited while the compositor stayed up; restarting",
                    stderr_tail=self.last_stderr_tail,
                )
                for t in pending:
                    t.cancel()
                drain_task.cancel()
                await self._graceful_kill(proc)
                self._record_crash_and_check()
                await asyncio.sleep(backoff)
                backoff = min(backoff * 2, _BACKOFF_MAX_SECONDS)
                continue
            drain_task.cancel()

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


def _browser_running() -> bool:
    """True when any of the known browser binaries has a live process.

    Deliberately a name probe rather than a PID: the browser is the
    compositor's grandchild and re-execs into several processes, so there is no
    single stable pid to hold. `pgrep -f` against the resolved binary names is
    the same mechanism the orphan sweep already uses.

    Errs toward TRUE on any uncertainty (pgrep missing, permission denied): a
    false "the browser is gone" would restart a working kiosk, which is worse
    than missing one failure.
    """
    for name in _BROWSER_CANDIDATES:
        try:
            res = subprocess.run(  # noqa: S603
                ["pgrep", "-f", name],
                capture_output=True,
                timeout=5,
                check=False,
            )
        except (OSError, subprocess.SubprocessError):
            return True
        if res.returncode == 0 and res.stdout.strip():
            return True
        if res.returncode not in (0, 1):
            return True
    return False


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

    if not await _wait_for_display():
        slog.info(
            "kiosk_hdmi_absent",
            msg="no DRM display after wait; HDMI kiosk skipped cleanly",
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
