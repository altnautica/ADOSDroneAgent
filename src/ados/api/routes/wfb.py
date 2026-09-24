"""The `video.wfb` config-merge helper the residual video tuning route shares.

Every WFB route (status, channel, TX power, auto-pair, local bind, unpair) is
served natively; this module holds no routes.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from ados.core.config.writer import merge_into_config
from ados.core.logging import get_logger
from ados.core.paths import CONFIG_YAML


def _persist_wfb_fields(updates: dict[str, Any]) -> bool:
    """Merge `updates` into the `video.wfb` block of the on-disk config so
    operator tuning survives a service restart.

    Routes through `ados.core.config.writer`, the one config writer: the merge
    happens on the document read inside the write lock, so a concurrent PUT
    cannot lose this update and the radio keys the Rust side owns
    (`reg_gate_strict`, `dfs_allowed`, `rendezvous_channel`) are untouched.
    """
    if not updates:
        return True
    result = merge_into_config(
        {"video": {"wfb": dict(updates)}}, path=Path(str(CONFIG_YAML))
    )
    if not result:
        get_logger("api.wfb").warning(
            "wfb_field_persist_failed", fields=sorted(updates), error=result.error
        )
    return bool(result)
