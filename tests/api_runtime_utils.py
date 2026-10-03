"""Test helpers for building API runtime doubles."""

from __future__ import annotations

from collections.abc import Mapping
from typing import Any
from unittest.mock import MagicMock

from ados.core.config import ADOSConfig
from ados.core.config.writer import ConfigWriteResult


def apply_dotted_values(config: ADOSConfig, values: Mapping[str, Any]) -> ADOSConfig:
    """``config`` with each dotted-path leaf in ``values`` set, re-validated."""
    document = config.model_dump()
    for dotted, value in values.items():
        cursor = document
        *parents, leaf = dotted.split(".")
        for part in parents:
            cursor = cursor.setdefault(part, {})
        cursor[leaf] = value
    return ADOSConfig(**document)


class _StateClientStub:
    """Stand-in for the state IPC client: a published snapshot dict."""

    def __init__(self, state: dict[str, Any] | None = None) -> None:
        self.state: dict[str, Any] = dict(state or {})


class ApiRuntimeTestDouble:
    """Small runtime object shaped like the API service runtime."""

    def __init__(
        self,
        *,
        config: ADOSConfig | None = None,
        state: dict[str, Any] | None = None,
    ) -> None:
        self.config = config or ADOSConfig()
        self.state_client = _StateClientStub(state)
        self.board_name = "test"
        self.model_manager = None
        self.pairing_manager = MagicMock()
        self.pairing_manager.is_paired = False
        # The setup-status builder reads the live pairing code when the
        # agent is unpaired; the real PairingManager returns a 6-char
        # string here, so the double must too or model validation fails.
        self.pairing_manager.get_or_create_code.return_value = "ABC234"

    def write_config(self, values: Mapping[str, Any]) -> ConfigWriteResult:
        """Apply the write to the in-memory config, as a re-read of disk would."""
        self.config = apply_dotted_values(self.config, values)
        return ConfigWriteResult(ok=True, changed=tuple(values))


def build_api_runtime(**kwargs: Any) -> ApiRuntimeTestDouble:
    return ApiRuntimeTestDouble(**kwargs)
