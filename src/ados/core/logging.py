"""Structured logging setup for ADOS Drone Agent."""

from __future__ import annotations

import logging
import os
import sys

import structlog

_SECRET_SUFFIXES = ("key", "code", "token", "password", "secret")
_REDACT_PREFIX = "redacted:"  # idempotency sentinel


def redact_value(key: str, value: str) -> str:
    """Redact one string value for ``key``.

    A secret-bearing key (one ending in, or equal to, a ``_SECRET_SUFFIXES``
    entry, case-insensitively) has its value replaced with
    ``redacted:len=<N>``, N its character count. Nothing computed from the
    content survives: a plaintext head plus an unkeyed digest let a reader of
    the logs recover a short secret by hashing candidates. An empty value, a
    non-secret key, or a value already carrying the ``redacted:`` sentinel
    passes through unchanged. The native store's redactor
    (``ados_protocol::logd::redact``) is byte-identical.
    """
    if not value or value.startswith(_REDACT_PREFIX):
        return value
    kl = key.lower()
    if not any(kl.endswith(s) or kl == s for s in _SECRET_SUFFIXES):
        return value
    return f"{_REDACT_PREFIX}len={len(value)}"


def redact_secrets(_logger, _method, event_dict):
    """structlog processor: redact any field whose key looks secret-bearing.

    Skips non-strings. Idempotent: already-redacted values pass through
    unchanged so a value that traverses the chain twice is not redacted again.
    """
    for k, v in list(event_dict.items()):
        if isinstance(v, str):
            event_dict[k] = redact_value(k, v)
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

    # Additively mirror records to the local logging-and-telemetry store.
    # The shipper is non-blocking and tolerates an absent socket (the usual
    # state on a box where the store is not installed), so installing it is
    # always safe and never disrupts the agent. The stderr/journald sink above
    # stays the always-on primary. Opt out with ADOS_LOGD_SHIP=0 if needed.
    if os.environ.get("ADOS_LOGD_SHIP", "1") != "0":
        try:
            from ados.core.logd_ship import install_logd_handler

            install_logd_handler()
        except Exception:
            # The shipper must never break logging setup. If it cannot install
            # (an unexpected import or thread-start failure), the primary sink
            # is unaffected and the agent runs normally.
            pass


def get_logger(name: str) -> structlog.stdlib.BoundLogger:
    """Get a named structlog logger."""
    return structlog.get_logger(name)
