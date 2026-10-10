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
from pathlib import Path
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
    """One answer from the control surface: the HTTP status and decoded body.

    ``base`` is the candidate base URL that answered, so a caller can report
    the port the agent is actually serving on.
    """

    status: int
    body: Any
    base: str = ""

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
    answered = ""
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
                    answered = base
                    break
                except httpx.ConnectError:
                    continue
    except httpx.HTTPError as exc:
        raise click.ClickException(f"agent request failed: {exc}") from exc
    if response is None:
        raise click.ClickException(
            "Agent is not running: no local control surface answered on "
            + ", ".join(api_bases())
            + ". Start it with: sudo systemctl start ados-supervisor"
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
    return AgentResponse(response.status_code, _decode(response), answered)


def call_json(
    method: str,
    path: str,
    *,
    json_body: Any = None,
    params: dict[str, str] | None = None,
    timeout: float | None = DEFAULT_TIMEOUT,
) -> dict[str, Any]:
    """:func:`call` for commands that only handle success: the body as a dict.

    A non-2xx answer raises :class:`click.ClickException` naming the status;
    a non-object body is wrapped as ``{"data": body}``.
    """
    response = call(method, path, json_body=json_body, params=params, timeout=timeout)
    if not response.ok:
        detail = response.body if isinstance(response.body, str) else json.dumps(response.body)
        raise click.ClickException(
            f"Agent API returned {response.status} for {path}: {detail[:160]}"
        )
    body = response.body
    return body if isinstance(body, dict) else {"data": body}


def download(path: str, dest: Path, *, timeout: float | None) -> AgentResponse:
    """Stream a GET into ``dest`` without buffering the body in memory.

    On success the answer's ``body`` is the number of bytes written; on an
    error status nothing is written and ``body`` is the decoded error. The
    same candidate-port, key and 401 handling as :func:`call`.
    """
    key = _load_pairing_key()
    headers = {"X-ADOS-Key": key.value} if key.value else {}
    request_timeout = httpx.Timeout(timeout, connect=CONNECT_TIMEOUT)
    try:
        with httpx.Client(timeout=request_timeout) as client:
            for base in api_bases():
                try:
                    with client.stream("GET", f"{base}{path}", headers=headers) as resp:
                        if resp.status_code == 401 and key.unreadable:
                            raise click.ClickException(
                                f"The agent requires its pairing key and {PAIRING_JSON} "
                                "is not readable by this user. Re-run the command with sudo."
                            )
                        if resp.status_code >= 400:
                            resp.read()
                            return AgentResponse(resp.status_code, _decode(resp), base)
                        written = 0
                        with dest.open("wb") as out:
                            for chunk in resp.iter_bytes():
                                out.write(chunk)
                                written += len(chunk)
                        return AgentResponse(resp.status_code, written, base)
                except httpx.ConnectError:
                    continue
    except httpx.HTTPError as exc:
        raise click.ClickException(f"agent request failed: {exc}") from exc
    raise click.ClickException(
        "Agent is not running: no local control surface answered on "
        + ", ".join(api_bases())
        + ". Start it with: sudo systemctl start ados-supervisor"
    )
