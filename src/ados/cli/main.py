"""Minimal public CLI for ADOS Drone Agent."""

from __future__ import annotations

import json
import os
import platform
import re
import shutil
import signal
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

import click
import httpx

from ados.cli import _ansi
from ados.core.paths import PAIRING_JSON

API_BASE = "http://localhost:8080"
PAIRING_STATE_PATH = PAIRING_JSON

# macOS runs a rootless, Rust-only workstation node: the control surface serves
# the native routes (/api/status, /api/pairing/*) but NOT the proxied setup
# facade (/api/v1/setup/status has no Python upstream there). The CLI reads the
# native routes on macOS.
IS_MACOS = platform.system() == "Darwin"


def _load_api_key() -> str | None:
    try:
        if PAIRING_STATE_PATH.exists():
            data = json.loads(PAIRING_STATE_PATH.read_text(encoding="utf-8"))
            key = data.get("api_key")
            return key if isinstance(key, str) else None
    except (OSError, ValueError, json.JSONDecodeError):
        pass
    return None


def _auth_headers() -> dict[str, str]:
    key = _load_api_key()
    return {"X-ADOS-Key": key} if key else {}


def _request(method: str, path: str, **kwargs: Any) -> dict[str, Any]:
    try:
        with httpx.Client(timeout=kwargs.pop("timeout", 8.0)) as client:
            response = client.request(
                method,
                f"{API_BASE}{path}",
                headers=_auth_headers(),
                **kwargs,
            )
            response.raise_for_status()
            data = response.json()
            return data if isinstance(data, dict) else {"data": data}
    except httpx.ConnectError:
        raise click.ClickException(
            "Agent is not running. Open the setup URL printed by the service, "
            "or start demo mode from the development entrypoint."
        ) from None
    except httpx.HTTPStatusError as exc:
        raise click.ClickException(
            f"Agent API returned {exc.response.status_code}: {exc.response.text[:160]}"
        ) from exc
    except httpx.HTTPError as exc:
        raise click.ClickException(str(exc)) from exc


def _native_status() -> dict[str, Any]:
    """Compose the status dict from the native control-surface routes.

    On a Rust-only node (the macOS workstation) the proxied setup facade is
    absent, so build the same shape ``_plain_status`` consumes out of the native
    ``/api/status`` + ``/api/pairing/info`` routes (both guaranteed-200). Fields
    a workstation does not have (an FC, a local video pipeline, a cloud relay)
    report honestly rather than being faked.
    """
    status = _request("GET", "/api/status")
    info = _request("GET", "/api/pairing/info")

    mdns_host = str(info.get("mdns_host") or "")
    paired = bool(info.get("paired"))
    fc_connected = status.get("fc_connected", info.get("fc_connected"))
    fc_port = status.get("fc_port") or info.get("fc_port")
    return {
        "device_name": info.get("name") or "ADOS",
        "profile": info.get("profile") or "workstation",
        "version": status.get("version") or info.get("version") or "?",
        "paired": paired,
        "pairing_code": info.get("pairing_code"),
        # A node that answers is installed + configured; "setup" is a drone
        # onboarding concept, not a workstation one, so report it complete.
        "completion_percent": 100,
        "network": {"api_port": 8080, "mdns_host": mdns_host, "hostname": mdns_host},
        "lan_host": mdns_host,
        "access_urls": [],
        "mavlink": {"connected": bool(fc_connected), "port": fc_port},
        "video": {"state": "n/a"},
        # The workstation config sets server.mode=local, so cloud relay is off.
        "cloud_choice": {"mode": "local"},
        "remote_access": {"status": "disabled"},
        "next_action": (
            "This node is connected to Mission Control."
            if paired
            else "In Mission Control, open Add a Node and enter this host."
        ),
    }


def _setup_status() -> dict[str, Any]:
    if IS_MACOS:
        return _native_status()
    return _request("GET", "/api/v1/setup/status")


def _viewer_url_from_whep(whep_url: str | None) -> str | None:
    """Derive the browser-clickable MediaMTX viewer URL from a WHEP URL.

    MediaMTX serves the JS player at ``http://host:port/<path>/`` and
    accepts the WebRTC SDP at ``http://host:port/<path>/whep``. The
    CLI prints both: ``whep`` for the GCS, viewer for an operator who
    wants to eyeball the stream in a browser.
    """
    if not whep_url:
        return None
    base = whep_url.rstrip("/")
    if base.endswith("/whep"):
        base = base[: -len("/whep")]
    return base + "/"


def _console_reach_urls(data: dict[str, Any]) -> list[str]:
    """Browser console URLs to open, best (LAN / mDNS) first.

    Prefers the server-composed setup/console URLs, then the resolved LAN host,
    with a constructed ``localhost`` only when nothing else is known. The
    reach-block renderer drops ``localhost`` as noise whenever a routable
    address is present, so a remote operator always sees a usable line first.
    """
    network = data.get("network", {}) or {}
    port = int(network.get("api_port", 8080) or 8080)
    urls: list[str] = []

    def _add_base(raw: str) -> None:
        # Normalize any console URL to its bare http://host:port form so every
        # reach line is consistent (the web UI at the root handles routing).
        if not raw.startswith("http"):
            return
        hostport = raw.split("//", 1)[1].split("/", 1)[0]
        base = f"http://{hostport}"
        if base not in urls:
            urls.append(base)

    for entry in data.get("access_urls") or []:
        raw = str(entry.get("url", ""))
        if raw and (entry.get("primary") or "/setup" in raw) and f":{port}" in raw:
            _add_base(raw)
    lan_host = data.get("lan_host") or network.get("mdns_host") or network.get("hostname")
    if lan_host and not _ansi.is_localhost(str(lan_host)):
        _add_base(f"http://{lan_host}:{port}")
    if not urls:
        _add_base(f"http://localhost:{port}")
    return urls


def _plain_status(data: dict[str, Any]) -> None:
    theme = _ansi.detect_theme()
    device = data.get("device_name", "?")
    profile = data.get("profile", "?")
    version = data.get("version", "?")
    click.echo(f"{_ansi.marker(theme, f'ADOS  {device} · {profile}')}   {theme.dim(f'v{version}')}")

    paired = bool(data.get("paired", False))
    code = data.get("pairing_code")
    if paired:
        pair_txt = f"{_ansi.dot(theme, 'ok')} {theme.ok('paired')}"
    elif code:
        pair_txt = (
            f"{_ansi.dot(theme, 'warn')} code {theme.bold(str(code))} "
            f"{theme.dim('(enter in Mission Control)')}"
        )
    else:
        pair_txt = f"{_ansi.dot(theme, 'pending')} {theme.dim('not paired')}"
    click.echo(f"  {theme.dim('setup')} {data.get('completion_percent', 0)}%    {pair_txt}")
    click.echo("")

    for line in _ansi.reach_block(theme, _console_reach_urls(data)):
        click.echo(line)
    click.echo("")

    mavlink = data.get("mavlink", {})
    fc = "connected" if mavlink.get("connected") else "not connected"
    port = mavlink.get("port")
    click.echo(_ansi.kv(theme, "MAVLink FC", fc + (f"  ({port})" if port else "")))
    if mavlink.get("tcp_url"):
        click.echo(_ansi.kv(theme, "MAVLink TCP", str(mavlink.get("tcp_url"))))
    if mavlink.get("websocket_url"):
        click.echo(_ansi.kv(theme, "MAVLink WS", str(mavlink.get("websocket_url"))))

    video = data.get("video", {})
    viewer_url = _viewer_url_from_whep(video.get("whep_url"))
    state = video.get("state", "unknown")
    click.echo(_ansi.kv(theme, "Video", state + (f"  {viewer_url}" if viewer_url else "")))

    cloud_choice = data.get("cloud_choice", {}) or {}
    cloud_paired = bool(cloud_choice.get("paired"))
    backend_url = str(cloud_choice.get("backend_url", "") or "")
    cloud_mode = str(cloud_choice.get("mode", "") or "")
    if cloud_paired and backend_url:
        cloud_txt = f"paired ({backend_url})"
    elif backend_url and cloud_mode != "local":
        cloud_txt = f"configured ({backend_url}, awaiting pair)"
    elif cloud_mode == "local":
        cloud_txt = "disabled (local mode)"
    else:
        cloud_txt = "not configured"
    click.echo(_ansi.kv(theme, "Cloud relay", cloud_txt))

    remote = data.get("remote_access", {}) or {}
    click.echo(_ansi.kv(theme, "Cloudflare", str(remote.get("status", "disabled"))))

    click.echo("")
    click.echo(theme.dim(f"Next: {data.get('next_action', 'Open setup in a browser')}"))


def _tui_binary() -> str | None:
    """Locate the ados-tui dashboard binary, if installed.

    The live terminal dashboard is the Rust ``ados-tui`` binary. It is
    installed alongside the agent; an override is honoured for development.
    """
    candidates: list[str] = []
    override = os.environ.get("ADOS_TUI_BIN")
    if override:
        candidates.append(override)
    candidates.append("/opt/ados/bin/ados-tui")
    on_path = shutil.which("ados-tui")
    if on_path:
        candidates.append(on_path)
    for candidate in candidates:
        if candidate and Path(candidate).is_file() and os.access(candidate, os.X_OK):
            return candidate
    return None


def _print_version(ctx: click.Context, _param: click.Parameter, value: bool) -> None:
    if not value or ctx.resilient_parsing:
        return
    from ados import __version__
    click.echo(__version__)
    ctx.exit()


@click.group(invoke_without_command=True)
@click.option(
    "--version",
    is_flag=True,
    is_eager=True,
    expose_value=False,
    callback=_print_version,
    help="Show the agent version and exit.",
)
@click.pass_context
def cli(ctx: click.Context) -> None:
    """ADOS Drone Agent."""
    if ctx.invoked_subcommand is None:
        # An interactive terminal hands off to the Rust dashboard binary; this
        # process is replaced by it. Without a TTY (or a dashboard binary),
        # fall back to the one-shot plain status.
        if sys.stdin.isatty() and sys.stdout.isatty():
            tui = _tui_binary()
            if tui:
                try:
                    os.execv(tui, [tui])  # replaces this process; does not return
                except OSError:
                    pass
        _plain_status(_setup_status())


@cli.command()
@click.option("--json", "as_json", is_flag=True, help="Output JSON for scripts.")
def status(as_json: bool) -> None:
    """Show agent setup, link, video, and service status."""
    data = _setup_status()
    if as_json:
        click.echo(json.dumps(data, indent=2))
        return
    _plain_status(data)


@cli.command()
def version() -> None:
    """Print the installed agent version (works when the service is down)."""
    from ados import __version__
    click.echo(__version__)


# The canonical install one-liner drives updates too: `ados update` re-runs it
# in upgrade mode, which is the ONE path that actually updates the agent (the
# Rust daemons + the CLI). On Linux it refetches the prebuilt installer and
# re-runs the full chain; on macOS it git-pulls the source and rebuilds. Both
# preserve identity/config. This deliberately replaces the old pip-wheel OTA,
# which only ever updated a Python wheel, not the Rust agent.
INSTALL_SH_URL = "https://raw.githubusercontent.com/altnautica/ADOSDroneAgent/main/scripts/install.sh"
REMOTE_VERSION_URL = (
    "https://raw.githubusercontent.com/altnautica/ADOSDroneAgent/main/src/ados/__init__.py"
)


def _installed_version() -> str:
    """The version this agent reports, preferring the running agent, then the
    recorded install result, then the CLI package."""
    try:
        data = _request("GET", "/api/version", timeout=4.0)
        v = data.get("version") or data.get("agent_version")
        if isinstance(v, str) and v:
            return v
    except click.ClickException:
        pass
    result = _read_install_result()
    if result and isinstance(result.get("version"), str):
        return str(result["version"])
    try:
        from ados import __version__

        return __version__
    except Exception:  # noqa: BLE001 - version display only, never fatal
        return "unknown"


def _latest_main_version() -> str | None:
    """The `__version__` on the tip of `main`, or None if it can't be fetched."""
    try:
        with httpx.Client(timeout=15.0, follow_redirects=True) as client:
            resp = client.get(REMOTE_VERSION_URL)
            resp.raise_for_status()
        match = re.search(r'__version__\s*=\s*"([^"]+)"', resp.text)
        return match.group(1) if match else None
    except (httpx.HTTPError, ValueError):
        return None


def _run_upgrade() -> None:
    """Fetch the canonical install.sh (latest main) and run it in upgrade mode.

    Linux needs root (the systemd install writes under /opt and /etc), so
    elevate with sudo when not already root; macOS installs per-user (build
    from source), so no sudo. The installer runs inline so its live progress
    stays attached to this terminal.
    """
    try:
        with httpx.Client(timeout=30.0, follow_redirects=True) as client:
            resp = client.get(INSTALL_SH_URL)
            resp.raise_for_status()
    except httpx.HTTPError as exc:
        raise click.ClickException(f"Could not fetch the installer: {exc}") from exc

    fd, script = tempfile.mkstemp(suffix="-ados-install.sh")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(resp.text)
        argv = ["bash", script, "--upgrade"]
        if platform.system() == "Linux" and os.geteuid() != 0:
            if shutil.which("sudo") is None:
                raise click.ClickException(
                    "Updating needs root on Linux. Re-run as: sudo ados update"
                )
            argv = ["sudo", *argv]
        try:
            completed = subprocess.run(argv, check=False)  # noqa: S603
        except OSError as exc:
            raise click.ClickException(f"Failed to launch the installer: {exc}") from exc
    finally:
        try:
            os.unlink(script)
        except OSError:
            pass

    if completed.returncode != 0:
        raise click.ClickException(f"Update finished with exit code {completed.returncode}.")


def _run_uninstall_via_installer(purge: bool, yes: bool) -> bool:
    """Confirm, then run the full-screen uninstall via the canonical install.sh.

    The Rust installer's `--uninstall` mode drives the same full-screen progress
    UI the install uses, so `ados uninstall` delegates to it (mirroring how
    `ados update` re-runs install.sh). Returns True when the installer actually
    ran; returns False when it could not be fetched or launched (offline), so the
    caller falls back to the in-process teardown. Raises `click.Abort` if the
    operator declines the confirmation.
    """
    if not yes:
        click.confirm("Uninstall the ADOS Drone Agent from this device?", abort=True)
    try:
        with httpx.Client(timeout=30.0, follow_redirects=True) as client:
            resp = client.get(INSTALL_SH_URL)
            resp.raise_for_status()
    except httpx.HTTPError:
        # Offline / unreachable: let the caller fall back to the local teardown.
        return False

    fd, script = tempfile.mkstemp(suffix="-ados-uninstall.sh")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(resp.text)
        # `--force` is how the installer requests a config purge on uninstall.
        argv = ["bash", script, "--uninstall"]
        if purge:
            argv.append("--force")
        if os.geteuid() != 0:
            if shutil.which("sudo") is None:
                raise click.ClickException(
                    "Uninstall needs root on Linux. Re-run as: sudo ados uninstall"
                )
            argv = ["sudo", *argv]
        try:
            completed = subprocess.run(argv, check=False)  # noqa: S603
        except OSError:
            return False
    finally:
        try:
            os.unlink(script)
        except OSError:
            pass

    if completed.returncode != 0:
        raise click.ClickException(
            f"Uninstall finished with exit code {completed.returncode}."
        )
    return True


@cli.command()
@click.option("--check-only", is_flag=True, help="Report the current + latest version, don't install.")
@click.option("--yes", "-y", is_flag=True, help="Update without an interactive prompt.")
@click.option("--json", "as_json", is_flag=True, help="Output JSON for scripts.")
def update(check_only: bool, yes: bool, as_json: bool) -> None:
    """Update the agent to the latest and restart it."""
    current = _installed_version()
    latest = _latest_main_version()
    available = bool(latest and latest != current)

    if as_json:
        click.echo(
            json.dumps(
                {
                    "current_version": current,
                    "latest_version": latest,
                    "update_available": available,
                },
                indent=2,
            )
        )
        return

    click.echo(f"Current version: {current}")
    if latest:
        click.echo(f"Latest (main):   {latest}")
    else:
        click.echo("Latest version:  could not check — updating to the latest main anyway.")

    if check_only:
        return

    if latest is not None and not available:
        click.echo("Already up to date.")
        return

    if not yes:
        click.confirm("Update the agent to the latest now?", abort=True)

    click.echo("Updating — this rebuilds and restarts the agent…")
    _run_upgrade()
    click.echo("Update complete.")


# Install orchestration contract paths. The installer writes a machine
# readable result file and per-step checkpoint markers under /var/lib/ados;
# these constants must stay aligned with the installer's result + checkpoint
# paths (crates/ados-installer/src/{result,checkpoint}.rs).
INSTALL_RESULT_PATH = Path("/var/lib/ados/install-result.json")
INSTALL_CHECKPOINT_DIR = Path("/var/lib/ados/install-checkpoints")
# The REQUIRED steps the full-agent install records a checkpoint for, in
# install order. Used by `ados install --status` to show done vs missing.
INSTALL_CHECKPOINT_STEPS = (
    "deps",
    "venv",
    "systemd",
    "global-symlinks",
)
# Canonical persisted installer path written by the install's
# persist_repo_artifacts step; falls back to the global `ados`-adjacent
# script when absent. --resume re-invokes this in resume mode.
INSTALLER_PERSISTED_PATH = Path("/opt/ados/source/scripts/install.sh")


def _read_install_result() -> dict[str, Any] | None:
    try:
        if INSTALL_RESULT_PATH.exists():
            data = json.loads(INSTALL_RESULT_PATH.read_text(encoding="utf-8"))
            return data if isinstance(data, dict) else None
    except (OSError, ValueError, json.JSONDecodeError):
        return None
    return None


def _checkpoint_state() -> tuple[list[str], list[str]]:
    """Return (done, missing) checkpoint step names, in install order."""
    done: list[str] = []
    missing: list[str] = []
    for step in INSTALL_CHECKPOINT_STEPS:
        marker = INSTALL_CHECKPOINT_DIR / f"{step}.done"
        if marker.exists():
            done.append(step)
        else:
            missing.append(step)
    return done, missing


@cli.command(name="install", hidden=True)
@click.option(
    "--status",
    "show_status",
    is_flag=True,
    default=False,
    help="Show the last install result and which checkpoints are done/missing.",
)
@click.option(
    "--resume",
    "do_resume",
    is_flag=True,
    default=False,
    help="Re-run the installer to finish any missing steps (resume a partial install).",
)
@click.option("--json", "as_json", is_flag=True, default=False, help="Output JSON for scripts.")
def install(show_status: bool, do_resume: bool, as_json: bool) -> None:
    """Inspect or resume the on-disk install.

    With --status, print the last install-result.json and the per-step
    checkpoint state. With --resume, re-run the installer in resume mode so
    a half-finished install (for example, one interrupted by a dropped SSH
    session) completes the missing steps. The installer is idempotent, so a
    resume on a healthy box is a fast no-op.
    """
    if do_resume:
        _install_resume()
        return

    # Default to --status when no action flag is given.
    _install_status(as_json=as_json)


def _install_status(*, as_json: bool) -> None:
    result = _read_install_result()
    done, missing = _checkpoint_state()

    if as_json:
        click.echo(
            json.dumps(
                {
                    "result": result,
                    "checkpoints": {"done": done, "missing": missing},
                },
                indent=2,
            )
        )
        return

    if result is None:
        click.echo(f"No install result recorded at {INSTALL_RESULT_PATH}.")
        click.echo("The installer has not finished a run on this box yet.")
    else:
        status = str(result.get("status", "unknown"))
        click.echo(f"Install status: {status}")
        click.echo(f"  Version:  {result.get('version', 'unknown')}")
        click.echo(f"  Profile:  {result.get('profile', 'unknown')}")
        click.echo(f"  Board:    {result.get('board', 'unknown')}")
        click.echo(f"  Kernel:   {result.get('kernelRelease', 'unknown')}")
        wfb = result.get("wfbModuleSource", "")
        click.echo(f"  WFB driver: {wfb or 'not installed'}")
        req = result.get("requiredFailures") or []
        failed = result.get("failedSteps") or []
        if req:
            click.echo(f"  Required failures: {', '.join(req)}")
        if failed:
            click.echo(f"  Failed steps:      {', '.join(failed)}")
        click.echo(f"  Recorded: {result.get('ts', 'unknown')}")

    click.echo("")
    click.echo(f"Checkpoints done ({len(done)}): {', '.join(done) or '<none>'}")
    click.echo(f"Checkpoints missing ({len(missing)}): {', '.join(missing) or '<none>'}")

    if missing or (result is not None and result.get("status") == "failed"):
        click.echo("")
        click.echo("Run 'sudo ados install --resume' to finish the missing steps.")


def _install_resume() -> None:
    if platform.system() != "Linux":
        raise click.ClickException("Resume is only supported on Linux installs.")
    if os.geteuid() != 0:
        raise click.ClickException("Resume must run as root: sudo ados install --resume")

    installer = _resolve_installer_path()
    if installer is None:
        raise click.ClickException(
            "Could not find the installer on disk. Re-run the install one-liner "
            "to recover this box."
        )

    click.echo(f"Resuming install via {installer} ...")
    # A plain re-run resumes by design: the completeness gate routes an
    # incomplete-but-present agent back through the (idempotent) install
    # body and skips finished checkpoints. The installer runs inline, so the
    # resume stays attached to this terminal and the operator sees progress
    # directly.
    env = dict(os.environ)
    try:
        completed = subprocess.run(  # noqa: S603
            ["/usr/bin/env", "bash", str(installer)],
            env=env,
            check=False,
        )
    except OSError as exc:
        raise click.ClickException(f"Failed to launch installer: {exc}") from exc

    if completed.returncode != 0:
        raise click.ClickException(
            f"Resume finished with exit code {completed.returncode}. "
            "Run 'ados install --status' for details."
        )
    click.echo("Resume complete.")


def _resolve_installer_path() -> Path | None:
    """Find the install.sh to re-run for a resume.

    Prefer the persisted copy under /opt/ados/source (written by the
    install's persist step). Fall back to a checkout adjacent to the
    running package source so a dev/editable install can resume too.
    """
    if INSTALLER_PERSISTED_PATH.exists():
        return INSTALLER_PERSISTED_PATH
    # Dev fallback: <repo>/scripts/install.sh relative to this module.
    try:
        repo_installer = Path(__file__).resolve().parents[3] / "scripts" / "install.sh"
        if repo_installer.exists():
            return repo_installer
    except (OSError, IndexError):
        pass
    return None


@cli.command()
@click.option("--purge", is_flag=True, default=False, help="Remove config as well.")
@click.option("--yes", "-y", is_flag=True, default=False, help="Skip confirmation prompt.")
def uninstall(purge: bool, yes: bool) -> None:
    """Uninstall ADOS Drone Agent from this system."""
    is_linux = platform.system() == "Linux"
    is_mac = platform.system() == "Darwin"
    if not is_linux and not is_mac:
        raise click.ClickException(f"Unsupported platform: {platform.system()}")

    if is_mac:
        _uninstall_macos(purge=purge, yes=yes)
        return
    # Prefer the full-screen installer-driven uninstall (the Rust `--uninstall`
    # mode renders the same live progress as the install); fall back to the
    # in-process teardown when the installer cannot be fetched/run (offline).
    # The delegation confirms + sudo-elevates itself, so the fallback runs with
    # yes=True to avoid a second prompt.
    if _run_uninstall_via_installer(purge=purge, yes=yes):
        return
    click.echo("Full-screen uninstaller unavailable; removing locally…", err=True)
    _uninstall_linux(purge=purge, yes=True)


def _stop_service_with_kill_fallback(service: str) -> None:
    """Best-effort stop with timeout + SIGKILL fallback.

    Why: stubborn child processes (or a wedged supervisor with hung
    children) can keep `systemctl stop <unit>` blocked past its
    timeout. The previous code passed timeout=30 and let
    `subprocess.TimeoutExpired` propagate, crashing the uninstall
    mid-transaction so symlinks and directories never got cleaned
    up. This helper bumps the graceful timeout to 60s, then on
    timeout escalates to `systemctl kill -s SIGKILL` and one more
    short stop. Any exception below this layer is logged and
    swallowed so the uninstall always continues to the cleanup
    phase.
    """
    if not shutil.which("systemctl"):
        return
    try:
        subprocess.run(
            ["systemctl", "stop", service],
            capture_output=True,
            timeout=60,
        )
        return
    except subprocess.TimeoutExpired:
        click.echo(
            f"  warn: stop {service} timed out, escalating to SIGKILL", err=True
        )
    except OSError as exc:
        click.echo(f"  warn: stop {service} failed: {exc}", err=True)
        return

    # Escalation: kill any remaining processes in the unit's cgroup,
    # then a short stop to clear systemd's tracking. Both are best-
    # effort — if even SIGKILL doesn't work, log and move on so the
    # filesystem cleanup still runs.
    try:
        subprocess.run(
            ["systemctl", "kill", "-s", "SIGKILL", service],
            capture_output=True,
            timeout=10,
        )
    except (subprocess.TimeoutExpired, OSError) as exc:
        click.echo(f"  warn: kill {service} failed: {exc}", err=True)
    try:
        subprocess.run(
            ["systemctl", "stop", service],
            capture_output=True,
            timeout=10,
        )
    except (subprocess.TimeoutExpired, OSError) as exc:
        click.echo(f"  warn: post-kill stop {service} failed: {exc}", err=True)


# The macOS workstation daemons registered as per-user LaunchAgents by the
# installer (macos.rs). `ados-tui` is installed but is not a daemon, so it has no
# LaunchAgent to boot out. The reverse-DNS labels are `co.ados.<tail>`.
_MACOS_DAEMONS = ("supervisor", "control", "compute", "cloud", "logd")


def _macos_ados_home() -> Path:
    """The per-user install root the macOS installer wrote (``$HOME/.ados``),
    honouring ``ADOS_HOME`` the same way the installer + paths module do."""
    override = os.environ.get("ADOS_HOME")
    if override:
        return Path(override)
    return Path.home() / ".ados"


def _uninstall_macos(*, purge: bool, yes: bool) -> None:
    """Tear down a macOS workstation node: boot every LaunchAgent out of the
    user's GUI domain, remove the plists, drop ``$HOME/.ados`` when purging, and
    (best-effort) pip-uninstall the CLI if it was pip-installed. Mirrors the
    installer's ``macos.rs`` uninstall so nothing is left running."""
    ados_home = _macos_ados_home()
    launch_agents = Path.home() / "Library" / "LaunchAgents"
    uid = os.getuid()

    if not yes:
        click.confirm("Uninstall ADOS from this Mac?", abort=True)
        # Mirror the Linux prompt: offer to purge identity + config when the
        # operator did not pass an explicit flag.
        if not purge and ados_home.exists():
            click.echo(f"  Config + identity live under {ados_home}.")
            click.echo("  Keep them for a re-install, or purge for a clean slate.")
            purge = click.confirm("Also remove ~/.ados?", default=False)

    pkg = "ados-drone-agent"

    def _stop_agents() -> str:
        stopped = 0
        for tail in _MACOS_DAEMONS:
            label = f"co.ados.{tail}"
            target = f"gui/{uid}/{label}"
            # Only boot out a genuinely-loaded job so a stale one does not error.
            probe = subprocess.run(
                ["launchctl", "print", target], capture_output=True, text=True
            )
            if probe.returncode == 0:
                subprocess.run(
                    ["launchctl", "bootout", target], capture_output=True, text=True
                )
                stopped += 1
        return f"{stopped} LaunchAgent{'s' if stopped != 1 else ''}"

    def _remove_plists() -> str:
        removed = 0
        if launch_agents.is_dir():
            for plist in sorted(launch_agents.glob("co.ados.*.plist")):
                try:
                    plist.unlink()
                    removed += 1
                except OSError:
                    pass
        return f"{removed} plist{'s' if removed != 1 else ''}"

    def _pip_remove() -> str:
        installer = "pip"
        for candidate, probe in (
            ("pipx", ["pipx", "list", "--short"]),
            ("uv", ["uv", "tool", "list"]),
        ):
            try:
                probed = subprocess.run(probe, capture_output=True, text=True, timeout=10)
                if probed.returncode == 0 and pkg in probed.stdout:
                    installer = candidate
                    break
            except FileNotFoundError:
                pass
        cmd = {
            "pipx": ["pipx", "uninstall", pkg],
            "uv": ["uv", "tool", "uninstall", pkg],
            "pip": [sys.executable, "-m", "pip", "uninstall", "-y", pkg],
        }[installer]
        result = subprocess.run(cmd, capture_output=True, text=True)
        # Not installed via a package manager (the installer builds from source
        # and does not pip-install) is fine — the node is already torn down.
        if result.returncode != 0:
            return "not package-managed (skipped)"
        return f"via {installer}"

    def _purge_home() -> str:
        if ados_home.exists():
            shutil.rmtree(ados_home, ignore_errors=True)
        return str(ados_home)

    steps: list[_ansi.Step] = [
        ("Stop ADOS LaunchAgents", _stop_agents),
        ("Remove LaunchAgent plists", _remove_plists),
        ("Remove ados CLI package", _pip_remove),
    ]
    if purge:
        steps.append(("Purge ~/.ados", _purge_home))

    theme = _ansi.detect_theme()
    results = _ansi.run_steps(
        theme, steps, title="Uninstalling ADOS", interactive=sys.stderr.isatty()
    )
    ok = all(r.ok for r in results)
    done = sum(1 for r in results if r.ok)
    glyph = theme.glyph_ok() if ok else theme.glyph_fail()
    summary = [
        f"{glyph} ADOS Workstation {'removed' if ok else 'removal finished with warnings'}",
        f"{done}/{len(results)} steps",
    ]
    if not purge:
        summary.append(f"kept: {ados_home}  (--purge to remove)")
    _ansi.print_card(theme, ok, summary)
    if not ok:
        raise click.ClickException("uninstall finished with warnings")


def _uninstall_linux(*, purge: bool, yes: bool) -> None:
    if os.geteuid() != 0:
        raise click.ClickException("Uninstall requires root. Run with sudo.")

    install_dir = Path("/opt/ados")
    config_dir = Path("/etc/ados")
    data_dir = Path("/var/ados")
    state_dir = Path("/var/lib/ados")
    log_dir = Path("/var/log/ados")
    motd_file = Path("/etc/update-motd.d/30-ados")
    systemd_dir = Path("/etc/systemd/system")

    # Discover all ados-* systemd unit files at runtime rather than
    # hardcoding a list. The installer's uninstall path
    # (crates/ados-installer/src/uninstall.rs) uses the same glob pattern;
    # keep this Python path in lockstep so the two uninstall surfaces never drift.
    unit_globs = ("ados-*.service", "ados-*.slice", "ados-*.target", "ados-*.timer")
    unit_files: list[Path] = []
    for pattern in unit_globs:
        unit_files.extend(sorted(systemd_dir.glob(pattern)))
    # Dropin .wants directories built by the supervisor + any orphan
    # multi-user.target.wants symlinks pointing into ados-*.
    wants_dirs = sorted(systemd_dir.glob("ados-*.service.wants"))
    target_wants_links = sorted((systemd_dir / "multi-user.target.wants").glob("ados-*"))

    # Tmpfiles, sysctl, modules-load, udev, and avahi dropins that the
    # install lays down outside /opt/ados. Without removing these, the
    # next fresh install can pick up a stale ados-display modules-load
    # line and load a wrong driver, or systemd-tmpfiles can recreate
    # /run/ados sockets that the new layout did not expect.
    dropin_files = [
        Path("/etc/tmpfiles.d/ados.conf"),
        Path("/etc/tmpfiles.d/ados-plugins.conf"),
        Path("/etc/sysctl.d/99-ados-video.conf"),
        Path("/etc/modules-load.d/ados-display.conf"),
        Path("/etc/udev/rules.d/50-ados-uvc-no-autosuspend.rules"),
        Path("/etc/udev/rules.d/99-ados-hardware.rules"),
        Path("/etc/udev/rules.d/99-ados-input.rules"),
        Path("/etc/udev/rules.d/99-ados-modem.rules"),
        Path("/etc/udev/rules.d/99-ados-wifi-powersave.rules"),
        Path("/etc/udev/rules.d/99-ados-usb-no-autosuspend.rules"),
        Path("/etc/udev/rules.d/99-ados-eth-no-eee.rules"),
        Path("/etc/NetworkManager/conf.d/99-ados-wifi-powersave.conf"),
        Path("/etc/systemd/logind.conf.d/99-ados-nosleep.conf"),
        Path("/etc/avahi/services/ados-gs-ap.service"),
    ]

    symlinks = [
        Path("/usr/local/bin/ados"),
        Path("/usr/local/bin/ados-agent"),
        Path("/usr/local/bin/ados-supervisor"),
    ]

    base_items = [
        *(f"systemd unit: {path.name}" for path in unit_files),
        *(f"dropin dir: {path}" for path in wants_dirs),
        *(f"target link: {path}" for path in target_wants_links),
        *(f"system dropin: {path}" for path in dropin_files if path.exists()),
        *(f"symlink: {path}" for path in symlinks if path.exists() or path.is_symlink()),
        *(f"dir: {path}" for path in (install_dir, data_dir, state_dir, log_dir) if path.exists()),
        *([f"login banner: {motd_file}"] if motd_file.exists() else []),
    ]
    if not base_items and not config_dir.exists():
        click.echo("Nothing to uninstall. ADOS Drone Agent is not installed.")
        return

    # Interactive purge prompt. When the operator did not pass --purge and
    # did not pass --yes, ask explicitly whether to keep the config so a
    # full clean uninstall does not require remembering the flag.
    if not yes and not purge and config_dir.exists():
        click.echo("The following will be removed:")
        for item in base_items:
            click.echo(f"  {item}")
        click.echo("")
        click.echo(f"Config directory: {config_dir}")
        click.echo("  Keep config: pairing key, device id, AP passphrase, custom YAML stay.")
        click.echo("  Purge config: full uninstall, next install starts from clean defaults.")
        purge = click.confirm("Also remove the config directory?", default=False)

    items = list(base_items)
    if purge and config_dir.exists():
        items.append(f"dir: {config_dir}")
    if not items:
        click.echo("Nothing to uninstall. ADOS Drone Agent is not installed.")
        return
    click.echo("")
    click.echo("The following will be removed:")
    for item in items:
        click.echo(f"  {item}")
    if not purge and config_dir.exists():
        click.echo(f"  keeping config: {config_dir}")
    if not yes:
        click.confirm("Proceed with uninstall?", abort=True)

    # Execute the teardown as a live checklist so a slow `systemctl stop`
    # (up to a minute for a stubborn unit) never looks frozen. Each step is
    # best-effort: failures are swallowed so the cleanup always continues,
    # matching the historical behavior. /var/lib/ados and /var/log/ados are
    # created at every install, so removing them here is symmetric; /run/ados
    # is tmpfs; config is gated by --purge.
    run_dir = Path("/run/ados")

    def _stop_services() -> str:
        for unit_file in unit_files:
            unit_name = unit_file.name
            if unit_name.endswith(".service"):
                _stop_service_with_kill_fallback(unit_name[: -len(".service")])
                try:
                    subprocess.run(
                        ["systemctl", "disable", unit_name], capture_output=True, timeout=10
                    )
                except (subprocess.TimeoutExpired, OSError):
                    pass
            else:
                # .slice, .target, .timer — stop best-effort.
                try:
                    subprocess.run(
                        ["systemctl", "stop", unit_name], capture_output=True, timeout=10
                    )
                except (subprocess.TimeoutExpired, OSError):
                    pass
        return f"{len(unit_files)} units"

    def _remove_units() -> str:
        for unit_file in unit_files:
            unit_file.unlink(missing_ok=True)
        for wants_dir in wants_dirs:
            shutil.rmtree(wants_dir, ignore_errors=True)
        for link in target_wants_links:
            try:
                link.unlink(missing_ok=True)
            except OSError:
                pass
        for dropin in dropin_files:
            if dropin.exists() or dropin.is_symlink():
                dropin.unlink(missing_ok=True)
        return ""

    def _reload_systemd() -> str:
        if shutil.which("systemctl"):
            for cmd in (["daemon-reload"], ["reset-failed"]):
                try:
                    subprocess.run(["systemctl", *cmd], capture_output=True, timeout=10)
                except (subprocess.TimeoutExpired, OSError):
                    pass
        if shutil.which("udevadm"):
            try:
                subprocess.run(
                    ["udevadm", "control", "--reload-rules"], capture_output=True, timeout=10
                )
            except (subprocess.TimeoutExpired, OSError):
                pass
        return ""

    def _remove_command() -> str:
        for path in symlinks:
            if path.exists() or path.is_symlink():
                path.unlink(missing_ok=True)
        return ""

    def _remove_files() -> str:
        for path in (install_dir, data_dir, state_dir, log_dir):
            if path.exists():
                shutil.rmtree(path, ignore_errors=True)
        if run_dir.exists():
            shutil.rmtree(run_dir, ignore_errors=True)
        if motd_file.exists():
            motd_file.unlink(missing_ok=True)
        return ""

    def _purge_config() -> str:
        shutil.rmtree(config_dir, ignore_errors=True)
        return ""

    steps: list[_ansi.Step] = [
        ("Stop ados services", _stop_services),
        ("Remove systemd units", _remove_units),
        ("Reload systemd and udev", _reload_systemd),
        ("Remove ados command", _remove_command),
        ("Remove files", _remove_files),
    ]
    if purge and config_dir.exists():
        steps.append(("Purge config", _purge_config))

    theme = _ansi.detect_theme()
    results = _ansi.run_steps(
        theme, steps, title="Uninstalling ADOS", interactive=sys.stderr.isatty()
    )
    ok = all(r.ok for r in results)
    done = sum(1 for r in results if r.ok)
    glyph = theme.glyph_ok() if ok else theme.glyph_fail()
    summary = [
        f"{glyph} ADOS Drone Agent {'removed' if ok else 'removal finished with warnings'}",
        f"{done}/{len(results)} steps",
    ]
    if not purge:
        summary.append(f"config kept: {config_dir}  (--purge to remove)")
    _ansi.print_card(theme, ok, summary)


def _resolve_router_bin() -> str | None:
    """Locate the native MAVLink router binary for demo mode.

    Order: ``ADOS_MAVLINK_ROUTER_BIN`` override, the installed path, then
    ``PATH``. Returns None when no binary is found.
    """
    override = os.environ.get("ADOS_MAVLINK_ROUTER_BIN")
    if override and Path(override).exists():
        return override
    installed = Path("/opt/ados/bin/ados-mavlink-router")
    if installed.exists():
        return str(installed)
    return shutil.which("ados-mavlink-router")


@cli.command(hidden=True)
@click.option("--port", default=8080, help="REST API port")
def demo(port: int) -> None:
    """Start in demo mode with simulated telemetry."""
    import asyncio

    from ados.core.config import load_config
    from ados.core.logging import configure_logging
    from ados.core.main import AgentApp

    config = load_config()
    config.server.mode = "disabled"
    config.pairing.state_path = str(Path.home() / ".ados" / "demo-pairing.json")
    config.pairing.convex_url = ""
    config.api.rest.port = port
    configure_logging(
        level=config.logging.level,
        drone_name=config.agent.name,
        device_id=config.agent.device_id,
    )

    # The native router owns the simulated FC + the state/command IPC sockets
    # that the agent reads. Spawn it in demo mode so telemetry flows; the
    # agent's state shims subscribe to /run/ados/state.sock it publishes.
    router_bin = _resolve_router_bin()
    router_proc: subprocess.Popen | None = None
    if router_bin is None:
        click.echo(
            "warn: ados-mavlink-router not found; demo telemetry will be empty. "
            "Set ADOS_MAVLINK_ROUTER_BIN or install the agent.",
            err=True,
        )
    else:
        router_proc = subprocess.Popen(
            [router_bin, "--demo"],
            env=os.environ.copy(),
        )
        click.echo(f"demo: started {router_bin} --demo (pid {router_proc.pid})")

    app = AgentApp(config, demo=True)
    loop = asyncio.new_event_loop()
    asyncio.set_event_loop(loop)
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, app.request_shutdown)
    try:
        loop.run_until_complete(app.start())
    except KeyboardInterrupt:
        app.request_shutdown()
    finally:
        loop.close()
        if router_proc is not None and router_proc.poll() is None:
            router_proc.terminate()
            try:
                router_proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                router_proc.kill()


# Wire subcommand groups. Done at import time so the entry point in
# pyproject.toml (ados = ados.cli.main:cli) sees the full command tree.
from ados.cli.hardware import hardware_group  # noqa: E402
from ados.cli.help import help_command  # noqa: E402
from ados.cli.logs import logs_group  # noqa: E402
from ados.cli.mcp import mcp_group  # noqa: E402
from ados.cli.network import network_group  # noqa: E402
from ados.cli.pair import pair, unpair  # noqa: E402
from ados.cli.plugin import plugin_group  # noqa: E402
from ados.cli.profile import profile_group  # noqa: E402
from ados.cli.radio import radio_group  # noqa: E402
from ados.cli.rust import rust_group  # noqa: E402

# Primitive operator commands stay on the primary help surface. The advanced
# groups keep working (log RCA, service toggles, plugins, …) but are hidden so
# the common path is uncluttered; `ados help` lists the primitives.
cli.add_command(pair)
cli.add_command(unpair)
cli.add_command(help_command)
cli.add_command(logs_group)

for _group in (
    hardware_group,
    mcp_group,
    network_group,
    plugin_group,
    profile_group,
    radio_group,
    rust_group,
):
    _group.hidden = True
    cli.add_command(_group)


if __name__ == "__main__":
    cli()
