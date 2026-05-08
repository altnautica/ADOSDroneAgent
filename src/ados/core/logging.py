"""Structured logging setup for ADOS Drone Agent."""

from __future__ import annotations

import hashlib
import logging
import sys

import structlog

_SECRET_SUFFIXES = ("key", "code", "token", "password", "secret")
_REDACT_PREFIX = "redacted:"  # idempotency sentinel


def redact_secrets(_logger, _method, event_dict):
    """structlog processor: hash any field whose key looks secret-bearing.

    Skips ints/bools/None. Idempotent: already-redacted values pass through
    unchanged so a value that traverses the chain twice does not double-hash.
    """
    for k, v in list(event_dict.items()):
        if not isinstance(v, str) or not v:
            continue
        if v.startswith(_REDACT_PREFIX):
            continue
        kl = k.lower()
        if any(kl.endswith(s) or kl == s for s in _SECRET_SUFFIXES):
            head = v[:4]
            digest = hashlib.sha256(v.encode("utf-8", errors="replace")).hexdigest()[:8]
            event_dict[k] = f"{_REDACT_PREFIX}{head}...{digest}"
    return event_dict


def configure_logging(
    level: str = "info",
    drone_name: str = "",
    device_id: str = "",
    json_output: bool = False,
) -> None:
    """Configure structlog with optional JSON output for journald."""
    log_level = getattr(logging, level.upper(), logging.INFO)

    if json_output:
        renderer = structlog.processors.JSONRenderer()
    else:
        renderer = structlog.dev.ConsoleRenderer(colors=sys.stderr.isatty())

    structlog.configure(
        processors=[
            structlog.contextvars.merge_contextvars,
            structlog.stdlib.filter_by_level,
            structlog.stdlib.add_logger_name,
            structlog.stdlib.add_log_level,
            structlog.processors.TimeStamper(fmt="iso"),
            structlog.processors.StackInfoRenderer(),
            structlog.processors.format_exc_info,
            redact_secrets,
            structlog.processors.UnicodeDecoder(),
            renderer,
        ],
        wrapper_class=structlog.stdlib.BoundLogger,
        context_class=dict,
        logger_factory=structlog.stdlib.LoggerFactory(),
        cache_logger_on_first_use=True,
    )

    logging.basicConfig(
        format="%(message)s",
        stream=sys.stderr,
        level=log_level,
    )

    # Bind global context
    if drone_name:
        structlog.contextvars.bind_contextvars(drone_name=drone_name)
    if device_id:
        structlog.contextvars.bind_contextvars(device_id=device_id)


def get_logger(name: str) -> structlog.stdlib.BoundLogger:
    """Get a named structlog logger."""
    return structlog.get_logger(name)
