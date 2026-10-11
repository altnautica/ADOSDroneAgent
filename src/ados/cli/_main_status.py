"""``ados status``: the setup summary read from the local control surface."""

from __future__ import annotations

import json
import platform
from typing import Any

import click

from ados.cli import _agent_http, _ansi, api_bases
from ados.core.paths import ADOS_RUN_DIR

# macOS runs a rootless, Rust-only workstation node: the control surface serves
# the native routes (/api/status, /api/pairing/*) but NOT the proxied setup
# facade (/api/v1/setup/status has no Python upstream there). The CLI reads the
# native routes on macOS.
IS_MACOS = platform.system() == "Darwin"


# The facade answers these when the residual setup API is absent (headless) or
# down; `ados status` then builds the summary from the native routes.
_SETUP_FALLBACK_STATUSES = frozenset({501, 502, 503})


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
    """One control-surface request; the parsed body dict, else a ClickException."""
    return _agent_http.call_json(
        method, path, json_body=kwargs.get("json"), timeout=kwargs.get("timeout", 8.0)
    )


def _read_native(path: str) -> tuple[dict[str, Any] | None, str]:
    """``(body, answering base)`` for a native read; ``None`` when it was not read."""
    try:
        response = _agent_http.call("GET", path, timeout=8.0)
    except click.ClickException:
        return None, ""
    if not response.ok or not isinstance(response.body, dict):
        return None, response.base
    return response.body, response.base


def _native_status() -> dict[str, Any]:
    """Compose the status dict from the native control-surface routes.

    Used when the proxied setup facade is absent (a Rust-only node) or its
    upstream is down. Built from ``/api/status/full``, ``/api/pairing/info``
    and ``server.mode`` in ``/api/config``; the API port is the one of the base
    that answered. Anything that could not be read prints as ``unknown``.
    """
    full, full_base = _read_native("/api/status/full")
    info, info_base = _read_native("/api/pairing/info")
    if full is None and info is None:
        raise click.ClickException(
            "Agent is not running. Start it with: sudo systemctl start ados-supervisor"
        )
    config, _ = _read_native("/api/config")
    status = full or {}
    pairing = info or {}
    base = full_base or info_base
    port: Any = "unknown"
    if base.rsplit(":", 1)[-1].isdigit():
        port = int(base.rsplit(":", 1)[-1])

    mdns_host = str(pairing.get("mdns_host") or "")
    paired = pairing.get("paired") if isinstance(pairing.get("paired"), bool) else None
    server = (config or {}).get("server")
    mode = server.get("mode") if isinstance(server, dict) else None
    video = status.get("video")
    fc_connected = status.get("fc_connected")
    return {
        "device_name": pairing.get("name") or "unknown",
        "profile": status.get("profile") or pairing.get("profile") or "unknown",
        "version": status.get("version") or pairing.get("version") or "unknown",
        "paired": paired,
        "pairing_code": pairing.get("pairing_code"),
        "completion_percent": "unknown",
        "network": {"api_port": port, "mdns_host": mdns_host, "hostname": mdns_host},
        "lan_host": mdns_host,
        "access_urls": [],
        "mavlink": {
            "connected": fc_connected if isinstance(fc_connected, bool) else None,
            "port": status.get("fc_port"),
        },
        "video": video if isinstance(video, dict) else {"state": "unknown"},
        "cloud_choice": {"mode": mode if isinstance(mode, str) and mode else "unknown"},
        "remote_access": {"status": "unknown"},
        "next_action": (
            "This node is connected to Mission Control."
            if paired
            else "In Mission Control, open Add a Node and enter this host."
        ),
    }


def _setup_status() -> dict[str, Any]:
    if IS_MACOS:
        return _native_status()
    response = _agent_http.call("GET", "/api/v1/setup/status", timeout=8.0)
    if response.ok and isinstance(response.body, dict):
        return response.body
    if response.status in _SETUP_FALLBACK_STATUSES:
        return _native_status()
    raise click.ClickException(
        f"Agent API returned {response.status}: {str(response.body)[:160]}"
    )


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
    raw_port = network.get("api_port", 8080)
    port = raw_port if isinstance(raw_port, int) else None
    suffix = f":{port}" if port is not None else ""
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
        if raw and (entry.get("primary") or "/setup" in raw) and suffix in raw:
            _add_base(raw)
    lan_host = data.get("lan_host") or network.get("mdns_host") or network.get("hostname")
    if lan_host and not _ansi.is_localhost(str(lan_host)):
        _add_base(f"http://{lan_host}{suffix}")
    if not urls:
        _add_base(f"http://localhost{suffix}")
    return urls


def _plain_status(data: dict[str, Any]) -> None:
    theme = _ansi.detect_theme()
    device = data.get("device_name", "?")
    profile = data.get("profile", "?")
    version = data.get("version", "?")
    click.echo(f"{_ansi.marker(theme, f'ADOS  {device} · {profile}')}   {theme.dim(f'v{version}')}")

    paired_raw = data.get("paired", False)
    code = data.get("pairing_code")
    if paired_raw is True:
        pair_txt = f"{_ansi.dot(theme, 'ok')} {theme.ok('paired')}"
    elif code:
        pair_txt = (
            f"{_ansi.dot(theme, 'warn')} code {theme.bold(str(code))} "
            f"{theme.dim('(enter in Mission Control)')}"
        )
    elif paired_raw is None:
        pair_txt = f"{_ansi.dot(theme, 'warn')} {theme.dim('pairing unknown')}"
    else:
        pair_txt = f"{_ansi.dot(theme, 'pending')} {theme.dim('not paired')}"
    completion = data.get("completion_percent", 0)
    completion_txt = f"{completion}%" if isinstance(completion, (int, float)) else str(completion)
    click.echo(f"  {theme.dim('setup')} {completion_txt}    {pair_txt}")
    click.echo("")

    for line in _ansi.reach_block(theme, _console_reach_urls(data)):
        click.echo(line)
    click.echo("")

    mavlink = data.get("mavlink", {})
    connected = mavlink.get("connected")
    fc = "unknown" if connected is None else ("connected" if connected else "not connected")
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
    backend_url = str(cloud_choice.get("backend_url", "") or "")
    cloud_mode = str(cloud_choice.get("mode", "") or "")
    if cloud_mode == "unknown":
        cloud_txt = "unknown"
    elif backend_url and cloud_mode != "local":
        cloud_txt = f"configured ({backend_url})"
    elif cloud_mode == "local":
        cloud_txt = "disabled (local mode)"
    else:
        cloud_txt = "not configured"
    click.echo(_ansi.kv(theme, "Cloud relay", cloud_txt))

    remote = data.get("remote_access", {}) or {}
    click.echo(_ansi.kv(theme, "Cloudflare", str(remote.get("status", "disabled"))))

    click.echo("")
    click.echo(theme.dim(f"Next: {data.get('next_action', 'Open setup in a browser')}"))


@click.command()
@click.option("--json", "as_json", is_flag=True, help="Output JSON for scripts.")
def status(as_json: bool) -> None:
    """Show agent setup, link, video, and service status."""
    data = _setup_status()
    if as_json:
        click.echo(json.dumps(data, indent=2))
        return
    _plain_status(data)
