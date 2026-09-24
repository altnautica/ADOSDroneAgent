"""Test helpers for building API runtime doubles."""

from __future__ import annotations

from typing import Any
from unittest.mock import MagicMock

from ados.core.config import ADOSConfig
from ados.core.config.writer import ConfigWriteResult


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

    def save_config(self) -> ConfigWriteResult:
        """Successful no-op persist so route tests drive the full flow off disk."""
        return ConfigWriteResult(ok=True)


def build_api_runtime(**kwargs: Any) -> ApiRuntimeTestDouble:
    return ApiRuntimeTestDouble(**kwargs)
