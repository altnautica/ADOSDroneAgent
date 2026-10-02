"""Minimal public CLI for ADOS Drone Agent."""

from __future__ import annotations

import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

import click
import httpx

from ados.cli import _ansi, api_bases, default_api_base
from ados.core.paths import ADOS_RUN_DIR, INSTALL_CHECKPOINT_DIR, INSTALL_RESULT, PAIRING_JSON
from ados.core.profile import _read_profile_conf_value

API_BASE = default_api_base()
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


def _answered_locally() -> bool:
    """Whether a ``_request`` can only have been served by the agent on this box.

    The video-binary marker ``_plain_status`` prints is a LOCAL filesystem fact,
    while every other value on that line comes from the HTTP response. It may
    only be attributed to a node this CLI actually reached on this machine — a
    remote agent's missing binary is not this box's marker, and this box's
    marker says nothing about the remote one. ``_request`` will accept an answer
    from any candidate base, so the honest predicate is that every candidate is
    loopback.
    """
    return all(_ansi.is_localhost(base) for base in api_bases())


def _request(method: str, path: str, **kwargs: Any) -> dict[str, Any]:
    timeout = kwargs.pop("timeout", 8.0)
    # Try each candidate control port; only a connection refusal falls through to
    # the next one. A wrong-port probe must never masquerade as "agent not
    # running" — the front may be on :8080 or the alternate :8082.
    bases = api_bases()
    try:
        for base in bases:
            try:
                with httpx.Client(timeout=timeout) as client:
                    response = client.request(
                        method,
                        f"{base}{path}",
                        headers=_auth_headers(),
                        **kwargs,
                    )
                response.raise_for_status()
                data = response.json()
                return data if isinstance(data, dict) else {"data": data}
            except httpx.ConnectError:
                continue  # this port refused; try the next candidate
        # Every candidate control port refused the connection.
        raise click.ClickException(
            "Agent is not running. Open the setup URL printed by the "
            "service, or start demo mode from the development entrypoint."
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
    # The agent advertises a same-origin relative path (`/whep`) with no host for
    # a clickable link. `ados status` runs on-box, so point the operator at the
    # local mediamtx viewer directly.
    if whep_url.startswith("/"):
        return "http://127.0.0.1:8889/main/"
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
    # A video block is only useful if it says WHY. `error` used to be
    # unreachable here because the status surface reported `running` off
    # mediamtx readiness alone; now that it can report a real failure, the
    # reason, the encoder identity, and a skipped-binary marker all print.
    detail = ""
    if video.get("reason"):
        detail += f"  ({video['reason']})"
    if video.get("encoder"):
        detail += f"  encoder={video['encoder']}"
    if viewer_url:
        detail += f"  {viewer_url}"
    if _answered_locally() and (ADOS_RUN_DIR / "video-binary-missing").exists():
        detail += "  [ados-video binary missing; re-run the installer]"
    click.echo(_ansi.kv(theme, "Video", state + detail))

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


def _upgrade_profile(override: str | None) -> str | None:
    """The profile an upgrade should pin, or ``None`` when it cannot be known.

    An upgrade must never be allowed to *change* what a box is. The installer
    resolves the profile itself when none is passed, and a gap in that
    resolution has re-profiled a live ground station to ``drone`` and left it
    in a reboot loop that needed a reflash to recover. The box already knows
    what it is — ``/etc/ados/profile.conf`` is written by both install.sh and
    the wizard — so an upgrade states it explicitly rather than relying on a
    default being right.

    An explicit ``--profile`` wins, for the one legitimate case: deliberately
    converting a box from one role to another.
    """
    if override:
        return override
    return _read_profile_conf_value()


def _run_upgrade(profile: str | None = None) -> None:
    """Fetch the canonical install.sh (latest main) and run it in upgrade mode.

    Linux needs root (the systemd install writes under /opt and /etc), so
    elevate with sudo when not already root; macOS installs per-user (build
    from source), so no sudo. The installer runs inline so its live progress
    stays attached to this terminal.

    ``profile`` is passed through to the installer when known, so an upgrade
    cannot silently re-profile the box.
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
        if profile:
            argv += ["--profile", profile]
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


# The copy of the Rust installer a successful install keeps on the box
# (`env::INSTALLED_INSTALLER` in crates/ados-installer), so uninstall runs
# offline through the one uninstall path.
LOCAL_INSTALLER_PATH = Path("/opt/ados/bin/ados-installer")


def _run_as_root(argv: list[str]) -> None:
    """Run ``argv`` inline (sudo-elevated when not root); a non-zero exit fails."""
    if os.geteuid() != 0:
        if shutil.which("sudo") is None:
            raise click.ClickException(
                "Uninstall needs root on Linux. Re-run as: sudo ados uninstall"
            )
        argv = ["sudo", *argv]
    try:
        completed = subprocess.run(argv, check=False)  # noqa: S603
    except OSError as exc:
        raise click.ClickException(f"Failed to launch the installer: {exc}") from exc
    if completed.returncode != 0:
        raise click.ClickException(
            f"Uninstall finished with exit code {completed.returncode}."
        )


def _run_uninstall_linux(*, purge: bool, yes: bool) -> None:
    """Confirm, then run the Rust installer's ``--uninstall`` mode.

    The installer owns the removal list and renders the same full-screen
    progress as the install, so it is the only uninstall path. A successful
    install keeps a copy at ``LOCAL_INSTALLER_PATH``, which runs offline; a box
    without that copy fetches the canonical install.sh, which downloads the
    installer and passes the flags through. Raises ``click.Abort`` if the
    operator declines the confirmation.
    """
    if not yes:
        click.confirm("Uninstall the ADOS Drone Agent from this device?", abort=True)
    # `--force` is how the installer requests a config purge on uninstall.
    flags = ["--uninstall", *(["--force"] if purge else [])]
    if LOCAL_INSTALLER_PATH.is_file() and os.access(LOCAL_INSTALLER_PATH, os.X_OK):
        _run_as_root([str(LOCAL_INSTALLER_PATH), *flags])
        return
    try:
        with httpx.Client(timeout=30.0, follow_redirects=True) as client:
            resp = client.get(INSTALL_SH_URL)
            resp.raise_for_status()
    except httpx.HTTPError as exc:
        raise click.ClickException(
            "The installer is not on this device and could not be downloaded "
            f"({exc}). Connect to the internet and run 'sudo ados uninstall' again."
        ) from exc

    fd, script = tempfile.mkstemp(suffix="-ados-uninstall.sh")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(resp.text)
        _run_as_root(["bash", script, *flags])
    finally:
        try:
            os.unlink(script)
        except OSError:
            pass


@cli.command()
@click.option("--check-only", is_flag=True, help="Report the current + latest version, don't install.")
@click.option("--yes", "-y", is_flag=True, help="Update without an interactive prompt.")
@click.option("--json", "as_json", is_flag=True, help="Output JSON for scripts.")
@click.option(
    "--profile",
    "profile_override",
    type=click.Choice(["drone", "ground-station", "workstation", "compute"]),
    default=None,
    help="Override the profile to install. Defaults to whatever this box already is.",
)
def update(check_only: bool, yes: bool, as_json: bool, profile_override: str | None) -> None:
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

    profile = _upgrade_profile(profile_override)
    if profile:
        click.echo(f"Profile:         {profile} (pinned, so the upgrade cannot change it)")
    else:
        click.echo(
            "Profile:         unknown — /etc/ados/profile.conf is missing, so the "
            "installer will resolve it. Pass --profile to be certain."
        )
    click.echo("Updating — this rebuilds and restarts the agent…")
    _run_upgrade(profile)
    click.echo("Update complete.")


# Install orchestration contract paths come from `ados.core.paths`, which
# resolves them per platform: the Linux FHS `/var/lib/ados` (the same literal
# `crates/ados-installer/src/env.rs` STATE_DIR carries, since the installer only
# ever runs on a target) or `~/.ados` on a macOS workstation. Hardcoding the
# Linux paths here made `ados install --status` report a path that cannot exist
# on macOS, and read nothing on a box where the installer had recorded a result.
# The checkpoint step list stays here because it is a CLI display contract, not a
# path: the REQUIRED steps the full-agent install records a checkpoint for, in
# install order, used by `ados install --status` to show done vs missing.
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
        if INSTALL_RESULT.exists():
            data = json.loads(INSTALL_RESULT.read_text(encoding="utf-8"))
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
        click.echo(f"No install result recorded at {INSTALL_RESULT}.")
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
    _run_uninstall_linux(purge=purge, yes=yes)


# The macOS workstation daemons registered as per-user LaunchAgents by the
# installer (macos.rs). `ados-tui` is installed but is not a daemon, so it has no
# LaunchAgent to boot out. The reverse-DNS labels are `co.ados.<tail>`.
_MACOS_DAEMONS = ("supervisor", "control", "cloud", "logd")


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


# Wire subcommand groups. Done at import time so the entry point in
# pyproject.toml (ados = ados.cli.main:cli) sees the full command tree.
from ados.cli.config import config_group  # noqa: E402
from ados.cli.diag import diag_group  # noqa: E402
from ados.cli.hardware import hardware_group  # noqa: E402
from ados.cli.help import help_command  # noqa: E402
from ados.cli.logs import logs_group  # noqa: E402
from ados.cli.mcp import mcp_group  # noqa: E402
from ados.cli.network import network_group  # noqa: E402
from ados.cli.pair import pair, unpair  # noqa: E402
from ados.cli.plugin import plugin_group  # noqa: E402
from ados.cli.profile import profile_group  # noqa: E402
from ados.cli.radio import radio_group  # noqa: E402
from ados.cli.record import record_group  # noqa: E402
from ados.cli.support import support_bundle  # noqa: E402

# Primitive operator commands stay on the primary help surface. The advanced
# groups keep working (log RCA, service toggles, plugins, …) but are hidden so
# the common path is uncluttered; `ados help` lists the primitives.
cli.add_command(pair)
cli.add_command(unpair)
cli.add_command(help_command)
cli.add_command(logs_group)
cli.add_command(support_bundle)

for _group in (
    config_group,
    diag_group,
    hardware_group,
    mcp_group,
    network_group,
    plugin_group,
    profile_group,
    radio_group,
    record_group,
):
    _group.hidden = True
    cli.add_command(_group)


if __name__ == "__main__":
    cli()
