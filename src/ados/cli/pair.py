"""``ados pair`` / ``ados unpair`` — connect this agent, and bind the radio.

One screen covers both connection planes:

* the **Mission Control node** connection — reach this agent over the LAN by
  hostname/IP (or a cloud pairing code), added in Mission Control → Add a Node;
* the **WFB radio bind** — the drone↔ground-station RF link (``--role``).

``ados unpair`` releases the Mission Control pairing (and, with ``--radio`` /
``--all``, wipes the WFB bind too).
"""

from __future__ import annotations

import platform
import re
import sys
from typing import Any

import click
import httpx

from ados.cli import _ansi

# The local-bind rendezvous can run up to ~120s; give the HTTP call headroom.
_WFB_BIND_TIMEOUT = 130.0
_IPV4 = re.compile(r"//(\d{1,3}(?:\.\d{1,3}){3})")


def _req(method: str, path: str, **kwargs: Any) -> tuple[int, dict[str, Any]]:
    """Call the local agent, returning ``(status_code, body)`` without raising.

    ``status_code == 0`` means the agent was unreachable. Non-raising so the
    caller can branch on 404 (feature absent on this profile) / 409 (bind busy).
    """
    from ados.cli.main import API_BASE, _load_api_key

    key = _load_api_key()
    headers = {"X-ADOS-Key": key} if key else {}
    try:
        with httpx.Client(timeout=kwargs.pop("timeout", 8.0)) as client:
            resp = client.request(method, f"{API_BASE}{path}", headers=headers, **kwargs)
            try:
                body = resp.json()
            except ValueError:
                body = {"text": resp.text}
            return resp.status_code, (body if isinstance(body, dict) else {"data": body})
    except httpx.HTTPError as exc:
        return 0, {"error": str(exc)}


def _interactive() -> bool:
    return sys.stderr.isatty()


def _node_hosts(status: dict[str, Any]) -> list[str]:
    """Reachable hostnames/IPs to paste into Mission Control → Add a Node.

    ``.local`` mDNS host first, then LAN IPs; localhost is never included (a
    remote operator cannot reach it)."""
    network = status.get("network", {}) or {}
    hosts: list[str] = []

    def _add(host: str | None) -> None:
        if host and host not in hosts and not _ansi.is_localhost(host):
            hosts.append(host)

    _add(status.get("lan_host"))
    _add(network.get("mdns_host"))
    _add(network.get("hostname"))
    for entry in status.get("access_urls") or []:
        match = _IPV4.search(str(entry.get("url", "")))
        if match and not match.group(1).startswith("127."):
            _add(match.group(1))
    return hosts


def _wfb_section(theme: _ansi.Theme) -> list[str]:
    lines = [_ansi.marker(theme, "Radio link (WFB)")]
    code, body = _req("GET", "/api/wfb/pair")
    if code == 0 or code >= 400 or not body:
        lines.append(f"  {theme.dim('not available on this profile')}")
        return lines
    role = body.get("role")
    peer = body.get("peer_device_id") or body.get("peer")
    fingerprint = body.get("fingerprint")
    auto = body.get("auto_pair")
    if auto is None:
        auto = body.get("auto")
    if body.get("paired"):
        lines.append(f"  {_ansi.dot(theme, 'ok')} {theme.ok('bound')}   {theme.dim(f'role {role}')}")
        if peer:
            lines.append(_ansi.kv(theme, "peer", str(peer)))
        if fingerprint:
            lines.append(_ansi.kv(theme, "fingerprint", str(fingerprint)))
    else:
        lines.append(f"  {_ansi.dot(theme, 'pending')} {theme.dim('not bound')}")
        lines.append(
            f"  {theme.dim('bind with:')}  {theme.accent('ados pair --role drone')}"
            f"  {theme.dim('or')}  {theme.accent('ados pair --role gs')}"
        )
    if auto is not None:
        lines.append(_ansi.kv(theme, "auto-pair", "on" if auto else "off"))
    return lines


def _pair_status() -> tuple[int, dict[str, Any]]:
    """Read the connection info the pair screen needs.

    On a Rust-only node (the macOS workstation) the proxied setup facade is
    absent, so read the native ``/api/pairing/info`` route and shape it into the
    small subset ``_show_pair_info`` + ``_node_hosts`` consume; elsewhere read the
    setup facade directly.
    """
    if platform.system() == "Darwin":
        code, info = _req("GET", "/api/pairing/info")
        if code == 0:
            return code, info
        mdns_host = str(info.get("mdns_host") or "")
        return code, {
            "paired": bool(info.get("paired")),
            "pairing_code": info.get("pairing_code"),
            "lan_host": mdns_host,
            "network": {"api_port": 8080, "mdns_host": mdns_host, "hostname": mdns_host},
            "access_urls": [],
        }
    return _req("GET", "/api/v1/setup/status")


def _show_pair_info(theme: _ansi.Theme) -> None:
    scode, status = _pair_status()
    if scode == 0:
        raise click.ClickException("Agent is not running on this host.")

    paired = bool(status.get("paired"))
    code = status.get("pairing_code")
    hosts = _node_hosts(status)
    port = int((status.get("network") or {}).get("api_port", 8080) or 8080)
    arrow = theme.accent(theme.glyph_arrow())

    out = [_ansi.marker(theme, "Connect to Mission Control")]
    if paired:
        out.append(
            f"  {_ansi.dot(theme, 'ok')} {theme.ok('paired')}   "
            f"{theme.dim('this agent is connected to Mission Control')}"
        )
    else:
        out.append(
            f"  {theme.dim('In Mission Control, open')} {theme.bold('Add a Node')} "
            f"{theme.dim('and enter this host:')}"
        )
        if hosts:
            for host in hosts:
                out.append(f"  {arrow}  {host}   {theme.dim(f'(port {port})')}")
        else:
            out.append(f"  {theme.dim('(no LAN address found — check the network)')}")
        if code:
            out.append("")
            out.append(f"  {theme.dim('Remote / cloud pairing code:')} {theme.bold(str(code))}")

    out.append("")
    out.extend(_wfb_section(theme))
    for line in out:
        click.echo(line)


def _do_wfb_bind(theme: _ansi.Theme, role: str, yes: bool) -> None:
    if not yes and _interactive():
        click.confirm(f"Bind the WFB radio as {role}?", default=True, abort=True)

    def _bind() -> str:
        code, body = _req(
            "POST", "/api/wfb/pair/local-bind", json={"role": role}, timeout=_WFB_BIND_TIMEOUT
        )
        if code == 0:
            raise RuntimeError(str(body.get("error", "agent not reachable")))
        if code == 409:
            raise RuntimeError("a bind session is already in progress")
        if code >= 400:
            raise RuntimeError(f"bind rejected ({code})")
        if body.get("state") != "paired":
            raise RuntimeError(str(body.get("reason") or f"bind ended: {body.get('state')}"))
        fingerprint = str(body.get("fingerprint", ""))
        return f"fingerprint {fingerprint}" if fingerprint else "paired"

    results = _ansi.run_steps(
        theme,
        [(f"Bind WFB radio ({role})", _bind)],
        title="Radio bind",
        interactive=_interactive(),
    )
    result = results[0]
    if result.ok:
        _ansi.print_card(
            theme,
            True,
            [f"{theme.glyph_ok()} Radio bound as {role}", result.detail, "", "Next: ados status"],
        )
    else:
        _ansi.print_card(theme, False, [f"{theme.glyph_fail()} Radio bind failed", result.detail])
        raise click.ClickException("radio bind failed")


def _unpair_node() -> str:
    code, body = _req("POST", "/api/pairing/unpair", timeout=15.0)
    if code == 0:
        raise RuntimeError(str(body.get("error", "agent not reachable")))
    if code >= 400:
        raise RuntimeError(f"unpair rejected ({code})")
    return ""


def _unpair_wfb() -> str:
    code, body = _req("POST", "/api/wfb/pair/unpair", timeout=30.0)
    if code == 0:
        raise RuntimeError(str(body.get("error", "agent not reachable")))
    if code >= 400:
        raise RuntimeError(f"wfb unpair rejected ({code})")
    role = body.get("role")
    return f"role {role}" if role else ""


@click.command()
@click.option(
    "--role",
    type=click.Choice(["drone", "gs"]),
    default=None,
    help="Bind the WFB radio in this role instead of showing connection info.",
)
@click.option("--yes", "-y", is_flag=True, help="Skip confirmation prompts.")
def pair(role: str | None, yes: bool) -> None:
    """Connect this agent to Mission Control, or bind the WFB radio (--role)."""
    theme = _ansi.detect_theme()
    if role:
        _do_wfb_bind(theme, role, yes)
        return
    _show_pair_info(theme)


@click.command()
@click.option("--radio", is_flag=True, help="Also wipe the WFB radio bind.")
@click.option("--all", "all_", is_flag=True, help="Unpair the node and the WFB radio.")
@click.option("--yes", "-y", is_flag=True, help="Skip the confirmation prompt.")
def unpair(radio: bool, all_: bool, yes: bool) -> None:
    """Release this agent from Mission Control (and optionally the WFB radio)."""
    theme = _ansi.detect_theme()
    do_radio = radio or all_
    if not yes:
        click.confirm(
            "Unpair this agent? Mission Control will lose access.", default=False, abort=True
        )

    steps: list[_ansi.Step] = [("Release Mission Control pairing", _unpair_node)]
    if do_radio:
        steps.append(("Wipe WFB radio bind", _unpair_wfb))
    results = _ansi.run_steps(theme, steps, title="Unpairing", interactive=_interactive())

    ok = all(r.ok for r in results)
    title = "Unpaired" if ok else "Unpair finished with warnings"
    glyph = theme.glyph_ok() if ok else theme.glyph_fail()
    _ansi.print_card(theme, ok, [f"{glyph} {title}", "pair again:  ados pair"])
    if not ok:
        raise click.ClickException("unpair finished with warnings")
