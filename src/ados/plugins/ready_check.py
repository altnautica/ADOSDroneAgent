"""The readiness probe grammar for a plugin-declared service.

A service's ``ready_check`` is plugin-authored text, and the probe runs on the
operator's say-so (the enable flow and every readiness poll), so it is parsed
into one of two shapes here, once, at manifest validation:

* An ``http://`` or ``https://`` URL on ``127.0.0.1`` with an explicit port:
  the agent GETs it and a 2xx is ready. Loopback only, because the request is
  made by the agent, not by the plugin.
* Anything else is an argv, split with POSIX shell quoting rules but never run
  by a shell. The supervisor runs it as the ``ados`` user inside the plugin's
  sandbox through ``systemd-run`` (see
  :func:`ados.plugins.systemd.probe_command`), never in the API process.

Control characters are refused in either form.
"""

from __future__ import annotations

import shlex
import unicodedata
from dataclasses import dataclass
from urllib.parse import urlsplit

#: The only host an HTTP probe may name.
LOOPBACK_HOST = "127.0.0.1"

#: How long one command probe may run before systemd stops it.
PROBE_TIMEOUT_S = 10


@dataclass(frozen=True)
class HttpProbe:
    """GET ``url``; ready on a 2xx status."""

    url: str


@dataclass(frozen=True)
class CommandProbe:
    """Run ``argv`` sandboxed; ready on exit status 0."""

    argv: tuple[str, ...]


def has_control_char(value: str) -> bool:
    """True when ``value`` carries any Unicode control character."""
    return any(unicodedata.category(ch) == "Cc" for ch in value)


def parse_ready_check(value: str) -> HttpProbe | CommandProbe:
    """Parse a ``ready_check`` value. Raises ``ValueError`` on anything else."""
    if has_control_char(value):
        raise ValueError("ready_check must not contain control characters")
    text = value.strip()
    if text.startswith(("http://", "https://")):
        parts = urlsplit(text)
        try:
            port = parts.port
        except ValueError as exc:
            raise ValueError(f"ready_check URL {text!r} has an invalid port") from exc
        if parts.hostname != LOOPBACK_HOST or port is None or parts.username or parts.password:
            raise ValueError(
                f"ready_check URL {text!r} must be http(s)://{LOOPBACK_HOST}:<port>/..."
            )
        return HttpProbe(url=text)
    argv = shlex.split(text)
    if not argv:
        raise ValueError("ready_check must not be empty")
    return CommandProbe(argv=tuple(argv))
