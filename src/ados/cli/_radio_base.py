"""Shared pieces of the ``ados radio`` CLI: the control-surface request
helper, signal formatters, and the ``radio`` click group itself."""

from __future__ import annotations

from typing import Any

import click

from ados.cli import _agent_http


def _request(
    method: str,
    path: str,
    *,
    raise_for_status: bool = True,
    **kwargs: Any,
) -> tuple[int, dict[str, Any]]:
    """One control-surface request: ``(status, body)``.

    An error status raises unless ``raise_for_status`` is False.
    """
    response = _agent_http.call(
        method,
        path,
        json_body=kwargs.get("json"),
        params=kwargs.get("params"),
        timeout=kwargs.get("timeout", 8.0),
    )
    body = response.body if not isinstance(response.body, str) else {"text": response.body}
    if raise_for_status and not response.ok:
        detail = body.get("detail") if isinstance(body, dict) else None
        raise click.ClickException(f"Agent API returned {response.status}: {detail or body}")
    return response.status, body if isinstance(body, dict) else {"data": body}


def _rssi_color(rssi: float | None) -> str:
    """Map RSSI dBm to a plain colour name. None and weak signals are red."""
    if rssi is None:
        return "red"
    if rssi >= -65.0:
        return "green"
    if rssi >= -78.0:
        return "yellow"
    return "red"


def _format_rssi(rssi: float | None) -> str:
    if rssi is None:
        return "n/a"
    return f"{rssi:.1f} dBm"


def _format_int_or_dash(value: Any) -> str:
    if value is None or value == 0:
        return "—" if value is None else "0"
    return str(value)


@click.group("radio", help="Inspect and tune the WFB-ng radio link.")
def radio_group() -> None:
    pass
