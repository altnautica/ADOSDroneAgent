"""IP validators, AP-subnet gate, and tiny file-loader helpers.

Stays minimal: these are leaf utilities pulled out so the request
models module can keep its imports thin and the bigger helper modules
do not have to redeclare them.
"""

from __future__ import annotations

import json
import re as _re
from pathlib import Path
from typing import Any

_IPV4_RE = _re.compile(
    r"^((25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.){3}(25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)$"
)


def _stock_confirm_token() -> str:
    """Confirmation token used when nothing is currently paired."""
    return "factory-reset-unpaired"


def _validate_ipv4(value: str) -> bool:
    return bool(_IPV4_RE.match(value))


def _validate_ipv4_cidr(value: str) -> bool:
    if "/" not in value:
        return False
    addr, _, prefix = value.partition("/")
    if not _validate_ipv4(addr):
        return False
    try:
        p = int(prefix)
    except ValueError:
        return False
    return 0 <= p <= 32


def _is_ap_subnet_client(host: str | None) -> bool:
    """True when the request came from the AP subnet 192.168.4.0/24.

    POC check: string-prefix match on the hotspot subnet. Loopback is
    also allowed so the agent itself and local tooling can mint a
    token for tests. Anything else is rejected with 403.
    """
    if not host:
        return False
    if host == "127.0.0.1" or host == "::1":
        return True
    return host.startswith("192.168.4.")


def _read_json_or_empty(path: Path) -> dict[str, Any]:
    try:
        if path.is_file():
            return json.loads(path.read_text(encoding="utf-8")) or {}
    except (OSError, ValueError):
        pass
    return {}


def _read_yaml_or_empty(path: Path) -> dict[str, Any]:
    """Read a YAML file into a dict. Returns {} on any failure.

    Used for ``/etc/ados/profile.conf`` which is written as YAML by
    ``profile_detect.write_profile_conf`` and by the ground-station
    install path.
    """
    try:
        if path.is_file():
            import yaml as _yaml
            data = _yaml.safe_load(path.read_text(encoding="utf-8"))
            return data if isinstance(data, dict) else {}
    except (OSError, ValueError):
        pass
    except Exception:
        # Corrupt YAML should not crash the endpoint; treat as empty.
        pass
    return {}


__all__ = [
    "_IPV4_RE",
    "_stock_confirm_token",
    "_validate_ipv4",
    "_validate_ipv4_cidr",
    "_is_ap_subnet_client",
    "_read_json_or_empty",
    "_read_yaml_or_empty",
]
