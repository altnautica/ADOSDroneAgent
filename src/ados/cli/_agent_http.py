"""HTTP client for CLI commands that drive the local native control surface.

The plugin lifecycle is served by ``ados-control``, so ``ados plugin`` talks to
it over loopback instead of mutating plugin state itself. Each candidate base
from :func:`ados.cli.api_bases` is tried in order; only a refused connection
falls through to the next one.

Authentication: a loopback caller is trusted on-box, and the pairing key from
``pairing.json`` rides along as ``X-ADOS-Key`` whenever this user can read it.
That file is root-owned ``0600``, so when the agent still answers 401 and the
key was unreadable, the error says to re-run with privilege rather than
surfacing a bare status code.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any

import click
import httpx

from ados.cli import api_bases
from ados.core.paths import PAIRING_JSON

#: Default per-request timeout, seconds. Installs pass ``None`` (no read limit)
#: because the agent answers only once a download and install have finished.
DEFAULT_TIMEOUT = 30.0
CONNECT_TIMEOUT = 5.0


@dataclass(frozen=True)
class AgentResponse:
    """One answer from the control surface: the HTTP status and decoded body."""

    status: int
    body: Any

    @property
    def ok(self) -> bool:
        return 200 <= self.status < 300


@dataclass(frozen=True)
class _PairingKey:
    value: str | None
    unreadable: bool


def _load_pairing_key() -> _PairingKey:
    try:
        text = PAIRING_JSON.read_text(encoding="utf-8")
    except FileNotFoundError:
        return _PairingKey(None, unreadable=False)
    except OSError:
        # Root-owned 0600 on a deployed node; a non-root operator lands here.
        return _PairingKey(None, unreadable=True)
    try:
        data = json.loads(text)
    except ValueError:
        return _PairingKey(None, unreadable=False)
    key = data.get("api_key") if isinstance(data, dict) else None
    return _PairingKey(key if isinstance(key, str) and key else None, unreadable=False)


def _decode(response: httpx.Response) -> Any:
    try:
        return response.json()
    except ValueError:
        return response.text


def call(
    method: str,
    path: str,
    *,
    json_body: Any = None,
    params: dict[str, str] | None = None,
    files: dict[str, tuple[str, bytes, str]] | None = None,
    timeout: float | None = DEFAULT_TIMEOUT,
) -> AgentResponse:
    """Send one request to the local control surface and return its answer.

    Non-2xx answers are returned, not raised, so a command can map the agent's
    ``{ok, code, kind, detail}`` envelope onto its own exit codes. Raises
    :class:`click.ClickException` when no candidate port accepts the connection,
    on a transport failure, and on a 401 (with the privilege hint when the
    pairing key could not be read).
    """
    key = _load_pairing_key()
    headers = {"X-ADOS-Key": key.value} if key.value else {}
    request_timeout = httpx.Timeout(timeout, connect=CONNECT_TIMEOUT)
    response: httpx.Response | None = None
    try:
        with httpx.Client(timeout=request_timeout) as client:
            for base in api_bases():
                try:
                    response = client.request(
                        method,
                        f"{base}{path}",
                        headers=headers,
                        json=json_body,
                        params=params,
                        files=files,
                    )
                    break
                except httpx.ConnectError:
                    continue
    except httpx.HTTPError as exc:
        raise click.ClickException(f"agent request failed: {exc}") from exc
    if response is None:
        raise click.ClickException(
            "Agent is not running: no local control surface answered on "
            + ", ".join(api_bases())
            + "."
        )
    if response.status_code == 401:
        if key.unreadable:
            raise click.ClickException(
                f"The agent requires its pairing key and {PAIRING_JSON} is not "
                "readable by this user. Re-run the command with sudo."
            )
        raise click.ClickException(
            f"The agent refused the request (401): {_decode(response)}"
        )
    return AgentResponse(response.status_code, _decode(response))
