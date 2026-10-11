"""HDMI kiosk: renderer plan, cage environment, and Chromium argv builders."""

from __future__ import annotations

import os
import shutil
from pathlib import Path

from ados.core.paths import ADOS_RUN_DIR
from ados.services.kiosk.display_probe import _resolve_drm_device

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

# XDG_RUNTIME_DIR for the cage launch, for exactly the same reason.
#
# The unit exports `/run/user/0`, but logind creates a per-user runtime dir only
# for a real login SESSION. A system service does not get one, so on an
# appliance box that path is simply absent and cage dies at startup with
# "Unable to open Wayland socket: Invalid argument" — while the cage process
# stays alive, so the unit still reads active and the screen is black.
#
# It appeared to work only on boxes where a desktop session had already created
# the directory; a freshly installed appliance has no session and no directory.
# Point it at a tmpfs path the agent creates and owns, mode 0700 as the spec
# requires, so it exists before cage needs it.
_CAGE_RUNTIME_DIR = str(ADOS_RUN_DIR / "kiosk" / "runtime")


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
        # Overrides the unit's /run/user/0, which does not exist for a system
        # service. Without this cage cannot bind its Wayland socket at all.
        "XDG_RUNTIME_DIR": _CAGE_RUNTIME_DIR,
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
