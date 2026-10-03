"""One identity per node: the device-id file wins, and the id is never truncated."""

from __future__ import annotations

from pathlib import Path

import pytest

from ados.core.config import ADOSConfig
from ados.core.identity import resolve_device_id

FULL_ID = "4e7a083410f5"


@pytest.fixture(autouse=True)
def _clean_env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("ADOS_DEVICE_ID", raising=False)
    monkeypatch.delenv("ADOS_DEVICE_ID_PATH", raising=False)


@pytest.fixture
def id_file(tmp_path: Path) -> Path:
    path = tmp_path / "device-id"
    path.write_text(f"{FULL_ID}\n")
    return path


@pytest.fixture
def no_file(tmp_path: Path) -> Path:
    return tmp_path / "absent"


def test_file_wins_over_env_and_config(
    id_file: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("ADOS_DEVICE_ID", "aaaaaaaaaaaa")
    assert resolve_device_id("4e7a0834", path=id_file) == FULL_ID


def test_env_path_override_locates_the_file(
    id_file: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("ADOS_DEVICE_ID_PATH", str(id_file))
    assert resolve_device_id("4e7a0834") == FULL_ID


@pytest.mark.parametrize("blank_file", [False, True])
def test_env_when_file_absent_or_blank(
    no_file: Path, blank_file: bool, monkeypatch: pytest.MonkeyPatch
) -> None:
    if blank_file:
        no_file.write_text("  \n")
    monkeypatch.setenv("ADOS_DEVICE_ID", " 0011aabbccdd ")
    assert resolve_device_id("cfg", path=no_file) == "0011aabbccdd"


def test_config_when_neither_file_nor_env(
    no_file: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("ADOS_DEVICE_ID", "  ")
    assert resolve_device_id(" 630e079b69d6 ", path=no_file) == "630e079b69d6"


def test_empty_when_nothing_resolves(no_file: Path) -> None:
    assert resolve_device_id(None, path=no_file) == ""
    assert resolve_device_id("  ", path=no_file) == ""


def test_twelve_char_id_is_not_truncated(id_file: Path) -> None:
    resolved = resolve_device_id(path=id_file)
    assert resolved == FULL_ID
    assert len(resolved) == 12


def test_config_model_replaces_short_configured_id_with_file(
    id_file: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("ADOS_DEVICE_ID_PATH", str(id_file))
    cfg = ADOSConfig.model_validate({"agent": {"device_id": FULL_ID[:8]}})
    assert cfg.agent.device_id == FULL_ID


def test_config_model_mints_full_id_at_the_resolved_path(
    no_file: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("ADOS_DEVICE_ID_PATH", str(no_file))
    minted = ADOSConfig().agent.device_id
    assert len(minted) == 12
    assert no_file.read_text().strip() == minted
    # A later load resolves the same identity instead of minting again.
    assert ADOSConfig().agent.device_id == minted
