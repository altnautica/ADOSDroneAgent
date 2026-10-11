"""Shared exit codes, agent-call helpers and the ``ados plugin`` group."""

from __future__ import annotations

import json
import re
import sys
from typing import Any, NoReturn
from urllib.parse import quote

import click

from ados.cli import _agent_http

EXIT_OK = 0
EXIT_GENERIC = 1
EXIT_MANIFEST_INVALID = 2
EXIT_SIGNATURE_INVALID = 3
EXIT_PERMISSION_DENIED = 4
EXIT_NOT_FOUND = 5
EXIT_WRONG_STATE = 6
EXIT_RESOURCE_LIMIT = 7
EXIT_COMPATIBILITY = 8

KIND_BY_CODE = {
    EXIT_OK: "ok",
    EXIT_GENERIC: "generic_failure",
    EXIT_MANIFEST_INVALID: "manifest_invalid",
    EXIT_SIGNATURE_INVALID: "signature_invalid",
    EXIT_PERMISSION_DENIED: "permission_denied",
    EXIT_NOT_FOUND: "plugin_not_found",
    EXIT_WRONG_STATE: "wrong_state",
    EXIT_RESOURCE_LIMIT: "resource_limit",
    EXIT_COMPATIBILITY: "compatibility_failed",
}

# A signer id names the agent's trusted key file `<signer-id>.pem`, so it is a
# plain file stem: no path separators and no leading dot.
_SIGNER_ID_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,63}")

# The agent's lifecycle error envelope ``{ok: false, code, kind, detail}``
# numbers its codes; this maps them onto the CLI exit codes above.
_EXIT_BY_AGENT_CODE = {
    10: EXIT_SIGNATURE_INVALID,
    11: EXIT_PERMISSION_DENIED,
    12: EXIT_MANIFEST_INVALID,
    13: EXIT_RESOURCE_LIMIT,
    14: EXIT_NOT_FOUND,
    17: EXIT_COMPATIBILITY,
    18: EXIT_COMPATIBILITY,  # incompatible: target profile, host arch, ...
    19: EXIT_PERMISSION_DENIED,  # refused: first-party-only gates
}

_UNSIGNED_HINT = (
    "The agent installs only signed archives unless its services run with "
    "ADOS_PLUGIN_REQUIRE_SIGNED=0 (developer mode)."
)


def _emit_ok(as_json: bool, data: dict[str, Any] | list[Any] | None = None) -> None:
    if as_json:
        click.echo(json.dumps({"ok": True, "code": 0, "kind": "ok", "data": data}))


def _emit_err(
    as_json: bool, code: int, message: str, hint: str | None = None
) -> None:
    if as_json:
        envelope = {
            "ok": False,
            "code": code,
            "kind": KIND_BY_CODE.get(code, "generic_failure"),
            "detail": message,
        }
        if hint:
            envelope["hint"] = hint
        click.echo(json.dumps(envelope))
    else:
        click.echo(f"Error: {message}", err=True)
        if hint:
            click.echo(f"Hint: {hint}", err=True)


def _fail(as_json: bool, code: int, message: str, hint: str | None = None) -> NoReturn:
    _emit_err(as_json, code, message, hint)
    sys.exit(code)


def _seg(value: str) -> str:
    """One URL path segment, escaped so an argument can never add another."""
    return quote(value, safe="")


def _call(method: str, path: str, as_json: bool, **kwargs: Any) -> _agent_http.AgentResponse:
    """One control-surface request; an unreachable or refusing agent exits here."""
    try:
        return _agent_http.call(method, path, **kwargs)
    except click.ClickException as exc:
        _fail(as_json, EXIT_GENERIC, exc.message)


def _failure(response: _agent_http.AgentResponse) -> tuple[int, str]:
    """The CLI exit code and message for an agent error answer."""
    body = response.body
    if isinstance(body, dict):
        code = body.get("code")
        message = str(body.get("detail") or f"agent answered HTTP {response.status}")
    else:
        # Not the lifecycle envelope (a proxy page, an empty 404): name the
        # status and keep only the start of the text.
        code = None
        text = str(body).strip()
        message = f"agent answered HTTP {response.status}" + (f": {text[:200]}" if text else "")
    if isinstance(code, int) and code in _EXIT_BY_AGENT_CODE:
        return _EXIT_BY_AGENT_CODE[code], message
    if message.startswith("incompatible:"):
        return EXIT_COMPATIBILITY, message
    if response.status == 404:
        return EXIT_NOT_FOUND, message
    return EXIT_GENERIC, message


def _agent(method: str, path: str, as_json: bool, **kwargs: Any) -> Any:
    """Call the control surface and return the body of a 2xx answer.

    An error answer is printed through the CLI envelope and exits with the
    mapped code.
    """
    response = _call(method, path, as_json, **kwargs)
    if not response.ok:
        exit_code, message = _failure(response)
        hint = _UNSIGNED_HINT if exit_code == EXIT_SIGNATURE_INVALID else None
        _fail(as_json, exit_code, message, hint)
    return response.body


def _is_builtin_install(install: dict[str, Any]) -> bool:
    return str(install.get("source_uri") or "").startswith("builtin:")


@click.group("plugin", help="Install, enable, and inspect plugins.")
def plugin_group() -> None:
    pass
