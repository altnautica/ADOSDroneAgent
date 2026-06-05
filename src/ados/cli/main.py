"""Minimal public CLI for ADOS Drone Agent."""

from __future__ import annotations

import json
import os
import platform
import shutil
import signal
import subprocess
import sys
from pathlib import Path
from typing import Any

import click
import httpx

from ados.core.paths import PAIRING_JSON

API_BASE = "http://localhost:8080"
PAIRING_STATE_PATH = PAIRING_JSON


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


def _setup_status() -> dict[str, Any]:
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


def _plain_status(data: dict[str, Any]) -> None:
    click.echo(f"ADOS Drone Agent {data.get('version', '?')}")
    click.echo(f"Device:  {data.get('device_name', '?')} ({data.get('device_id', '?')})")
    click.echo(f"Profile: {data.get('profile', '?')}")
    click.echo(f"Setup:   {data.get('completion_percent', 0)}%")

    paired = bool(data.get("paired", False))
    code = data.get("pairing_code")
    if paired:
        click.echo("Pair:    paired")
    elif code:
        click.echo(f"Pair:    code {code}  (enter this in Mission Control)")
    else:
        click.echo("Pair:    not paired")
    click.echo("")

    # Open-setup URL. Prefer a LAN-routable form so the line is paste-
    # ready from a separate workstation.
    network = data.get("network", {})
    lan_host = data.get("lan_host") or network.get("mdns_host") or network.get(
        "hostname", ""
    )
    api_port = int(network.get("api_port", 8080) or 8080)
    if lan_host:
        click.echo(f"Open setup: http://{lan_host}:{api_port}/setup")
    else:
        urls = data.get("access_urls", [])
        primary = next((u.get("url") for u in urls if u.get("primary")), None)
        click.echo(f"Open setup: {primary or 'http://localhost:8080/setup'}")

    mavlink = data.get("mavlink", {})
    fc_connected = bool(mavlink.get("connected"))
    click.echo(
        "MAVLink FC: "
        + ("connected" if fc_connected else "not connected")
        + (f"  ({mavlink.get('port')})" if mavlink.get("port") else "")
    )
    if mavlink.get("tcp_url"):
        click.echo(f"MAVLink TCP: {mavlink.get('tcp_url')}")
    if mavlink.get("websocket_url"):
        click.echo(f"MAVLink WS:  {mavlink.get('websocket_url')}")

    video = data.get("video", {})
    whep_url = video.get("whep_url")
    viewer_url = _viewer_url_from_whep(whep_url)
    state = video.get("state", "unknown")
    if viewer_url:
        click.echo(f"Video:      {state}  viewer {viewer_url}")
    else:
        click.echo(f"Video:      {state}")

    cloud_choice = data.get("cloud_choice", {}) or {}
    cloud_paired = bool(cloud_choice.get("paired"))
    backend_url = str(cloud_choice.get("backend_url", "") or "")
    cloud_mode = str(cloud_choice.get("mode", "") or "")
    if cloud_paired and backend_url:
        click.echo(f"Cloud relay: paired ({backend_url})")
    elif backend_url and cloud_mode != "local":
        click.echo(f"Cloud relay: configured ({backend_url}, awaiting pair)")
    elif cloud_mode == "local":
        click.echo("Cloud relay: disabled (local mode)")
    else:
        click.echo("Cloud relay: not configured")

    remote = data.get("remote_access", {}) or {}
    cloudflare_status = remote.get("status", "disabled")
    click.echo(f"Cloudflare: {cloudflare_status}")

    click.echo("")
    click.echo(f"Next: {data.get('next_action', 'Open setup in a browser')}")


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


@cli.command()
@click.option("--check-only", is_flag=True, help="Check for updates without installing.")
@click.option("--yes", "-y", is_flag=True, help="Install without an interactive prompt.")
@click.option("--json", "as_json", is_flag=True, help="Output JSON for scripts.")
def update(check_only: bool, yes: bool, as_json: bool) -> None:
    """Check for and optionally install an agent update."""
    current = _request("GET", "/api/ota")
    checked = _request("POST", "/api/ota/check", timeout=30.0)
    if as_json:
        click.echo(json.dumps({"current": current, "check": checked}, indent=2))
        return

    version = current.get("current_version", current.get("version", "?"))
    click.echo(f"Current version: {version}")
    if checked.get("status") != "update_available":
        click.echo("Already up to date.")
        return

    new_version = checked.get("version", "?")
    click.echo(f"Update available: {new_version}")
    if check_only:
        return
    if not yes:
        click.confirm(f"Install {new_version} now?", abort=True)

    from ados.cli import update_ui

    # Interactive (a live phase checklist + download bar + closing card) when
    # stderr is a terminal; plain line output when piped / `--json`.
    update_ui.run(
        _request,
        current_version=str(version),
        new_version=str(new_version),
        interactive=sys.stderr.isatty() and not as_json,
    )


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


@cli.command(name="install")
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
        _uninstall_macos(yes=yes)
        return
    _uninstall_linux(purge=purge, yes=yes)


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


def _uninstall_macos(*, yes: bool) -> None:
    if not yes:
        click.confirm("Uninstall ados-drone-agent from this system?", abort=True)
    pkg = "ados-drone-agent"
    installer = "pip"
    for candidate, cmd in (
        ("pipx", ["pipx", "list", "--short"]),
        ("uv", ["uv", "tool", "list"]),
    ):
        try:
            result = subprocess.run(cmd, capture_output=True, text=True, timeout=10)
            if result.returncode == 0 and pkg in result.stdout:
                installer = candidate
                break
        except FileNotFoundError:
            pass
    cmd = {
        "pipx": ["pipx", "uninstall", pkg],
        "uv": ["uv", "tool", "uninstall", pkg],
        "pip": [sys.executable, "-m", "pip", "uninstall", "-y", pkg],
    }[installer]
    click.echo(f"Running: {' '.join(cmd)}")
    result = subprocess.run(cmd, capture_output=True, text=True)
    if result.returncode != 0:
        raise click.ClickException(result.stderr.strip() or "Uninstall failed")
    click.echo("ADOS Drone Agent uninstalled.")


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

    # Stop + disable + remove each unit. systemctl disable on a unit
    # that was never enabled is harmless.
    for unit_file in unit_files:
        unit_name = unit_file.name
        if unit_name.endswith(".service"):
            _stop_service_with_kill_fallback(unit_name[: -len(".service")])
            try:
                subprocess.run(
                    ["systemctl", "disable", unit_name],
                    capture_output=True,
                    timeout=10,
                )
            except (subprocess.TimeoutExpired, OSError) as exc:
                click.echo(f"  warn: disable {unit_name} skipped: {exc}", err=True)
        else:
            # .slice, .target, .timer — stop best-effort.
            try:
                subprocess.run(
                    ["systemctl", "stop", unit_name],
                    capture_output=True,
                    timeout=10,
                )
            except (subprocess.TimeoutExpired, OSError) as exc:
                click.echo(f"  warn: stop {unit_name} skipped: {exc}", err=True)
        unit_file.unlink(missing_ok=True)

    for wants_dir in wants_dirs:
        shutil.rmtree(wants_dir, ignore_errors=True)
    for link in target_wants_links:
        try:
            link.unlink(missing_ok=True)
        except OSError as exc:
            click.echo(f"  warn: removing {link} failed: {exc}", err=True)
    for dropin in dropin_files:
        if dropin.exists() or dropin.is_symlink():
            dropin.unlink(missing_ok=True)

    if shutil.which("systemctl"):
        for cmd in (["daemon-reload"], ["reset-failed"]):
            try:
                subprocess.run(
                    ["systemctl", *cmd], capture_output=True, timeout=10
                )
            except (subprocess.TimeoutExpired, OSError) as exc:
                click.echo(f"  warn: systemctl {' '.join(cmd)} skipped: {exc}", err=True)
    if shutil.which("udevadm"):
        try:
            subprocess.run(
                ["udevadm", "control", "--reload-rules"],
                capture_output=True,
                timeout=10,
            )
        except (subprocess.TimeoutExpired, OSError) as exc:
            click.echo(f"  warn: udevadm reload skipped: {exc}", err=True)

    for path in symlinks:
        if path.exists() or path.is_symlink():
            path.unlink(missing_ok=True)
    # Always wipe install + data + state + log dirs. /var/lib/ados and
    # /var/log/ados are created by setup_state_dirs at every install, so
    # removing them here is symmetric. Config is gated by --purge below.
    for path in (install_dir, data_dir, state_dir, log_dir):
        if path.exists():
            shutil.rmtree(path, ignore_errors=True)
    # /run/ados is tmpfs; best-effort.
    run_dir = Path("/run/ados")
    if run_dir.exists():
        shutil.rmtree(run_dir, ignore_errors=True)
    if motd_file.exists():
        motd_file.unlink(missing_ok=True)
    if purge and config_dir.exists():
        shutil.rmtree(config_dir, ignore_errors=True)
    click.echo("ADOS Drone Agent uninstalled.")


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


# Wire subcommand groups. Done at import time so the entry point in
# pyproject.toml (ados = ados.cli.main:cli) sees the full command tree.
from ados.cli.hardware import hardware_group  # noqa: E402
from ados.cli.logs import logs_group  # noqa: E402
from ados.cli.network import network_group  # noqa: E402
from ados.cli.plugin import plugin_group  # noqa: E402
from ados.cli.profile import profile_group  # noqa: E402
from ados.cli.radio import radio_group  # noqa: E402
from ados.cli.rust import rust_group  # noqa: E402

cli.add_command(hardware_group)
cli.add_command(logs_group)
cli.add_command(network_group)
cli.add_command(plugin_group)
cli.add_command(profile_group)
cli.add_command(radio_group)
cli.add_command(rust_group)


if __name__ == "__main__":
    cli()
