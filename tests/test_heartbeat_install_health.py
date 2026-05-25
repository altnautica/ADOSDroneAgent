"""Tests for the install-health + kernel/radio-module heartbeat fields.

The heartbeat surfaces the kernel release, the provenance of the loaded
WFB radio module, and the outcome of the last install/upgrade so the GCS
can flag a degraded or failed install without an SSH session. These tests
pin the contract:

* ``kernelRelease`` is always a non-empty string.
* ``wfbModuleSource`` is always one of ``{"prebuilt", "dkms", "none"}``
  and resolves across the tmpfs-reboot gap (live modinfo path is the
  source of truth, the breadcrumb is a fast hint, the install record is
  the fallback).
* ``installStatus`` / ``installVersion`` / ``failedSteps`` map through
  from the install-result record when present, and fall back to
  defaults (with no exception) when the file is absent or garbage.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest

from ados.core.config import ADOSConfig
from ados.core.main import AgentApp
from ados.core.main import heartbeat_payload as hp


def _fresh_app() -> AgentApp:
    """Build an AgentApp without running .start() (no asyncio loop)."""
    return AgentApp(ADOSConfig())


def _no_install_sources(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    """Point every install-health source at absent paths + dead modinfo."""
    monkeypatch.setattr(hp, "INSTALL_RESULT", tmp_path / "install-result.json")
    monkeypatch.setattr(hp, "WFB_MODULE_SOURCE", tmp_path / "wfb-module-source")
    monkeypatch.setattr(hp, "_wfb_module_source_from_modinfo", lambda: None)


# ---------------------------------------------------------------------------
# kernelRelease
# ---------------------------------------------------------------------------


def test_kernel_release_always_present_nonempty(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    _no_install_sources(monkeypatch, tmp_path)
    payload = _fresh_app()._build_heartbeat_payload()
    assert isinstance(payload["kernelRelease"], str)
    assert payload["kernelRelease"]


# ---------------------------------------------------------------------------
# wfbModuleSource
# ---------------------------------------------------------------------------


def test_wfb_module_source_in_allowed_set(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    _no_install_sources(monkeypatch, tmp_path)
    payload = _fresh_app()._build_heartbeat_payload()
    assert payload["wfbModuleSource"] in {"prebuilt", "dkms", "none"}


def test_wfb_module_source_none_when_no_sources(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    _no_install_sources(monkeypatch, tmp_path)
    payload = _fresh_app()._build_heartbeat_payload()
    assert payload["wfbModuleSource"] == "none"


def test_wfb_module_source_modinfo_prebuilt(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """A modinfo path under updates/ classifies as prebuilt."""

    def fake_run(*_args, **_kwargs):  # noqa: ANN002, ANN003
        return subprocess.CompletedProcess(
            args=["modinfo"],
            returncode=0,
            stdout="/lib/modules/6.1.0/updates/8812eu.ko\n",
            stderr="",
        )

    monkeypatch.setattr(hp.subprocess, "run", fake_run)
    monkeypatch.setattr(hp, "INSTALL_RESULT", tmp_path / "install-result.json")
    monkeypatch.setattr(hp, "WFB_MODULE_SOURCE", tmp_path / "wfb-module-source")
    payload = _fresh_app()._build_heartbeat_payload()
    assert payload["wfbModuleSource"] == "prebuilt"


def test_wfb_module_source_modinfo_dkms_extra(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """A modinfo path under extra/ classifies as dkms."""

    def fake_run(*_args, **_kwargs):  # noqa: ANN002, ANN003
        return subprocess.CompletedProcess(
            args=["modinfo"],
            returncode=0,
            stdout="/lib/modules/6.1.0/extra/8812eu.ko\n",
            stderr="",
        )

    monkeypatch.setattr(hp.subprocess, "run", fake_run)
    monkeypatch.setattr(hp, "INSTALL_RESULT", tmp_path / "install-result.json")
    monkeypatch.setattr(hp, "WFB_MODULE_SOURCE", tmp_path / "wfb-module-source")
    payload = _fresh_app()._build_heartbeat_payload()
    assert payload["wfbModuleSource"] == "dkms"


def test_wfb_module_source_modinfo_dkms_in_path(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """A modinfo path containing 'dkms' classifies as dkms."""

    def fake_run(*_args, **_kwargs):  # noqa: ANN002, ANN003
        return subprocess.CompletedProcess(
            args=["modinfo"],
            returncode=0,
            stdout="/var/lib/dkms/8812eu/1.0/6.1.0/aarch64/module/8812eu.ko\n",
            stderr="",
        )

    monkeypatch.setattr(hp.subprocess, "run", fake_run)
    monkeypatch.setattr(hp, "INSTALL_RESULT", tmp_path / "install-result.json")
    monkeypatch.setattr(hp, "WFB_MODULE_SOURCE", tmp_path / "wfb-module-source")
    payload = _fresh_app()._build_heartbeat_payload()
    assert payload["wfbModuleSource"] == "dkms"


def test_wfb_module_source_breadcrumb_fallback(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """modinfo unhelpful → tmpfs breadcrumb is consulted next."""
    crumb = tmp_path / "wfb-module-source"
    crumb.write_text("dkms\n")
    monkeypatch.setattr(hp, "_wfb_module_source_from_modinfo", lambda: None)
    monkeypatch.setattr(hp, "INSTALL_RESULT", tmp_path / "install-result.json")
    monkeypatch.setattr(hp, "WFB_MODULE_SOURCE", crumb)
    payload = _fresh_app()._build_heartbeat_payload()
    assert payload["wfbModuleSource"] == "dkms"


def test_wfb_module_source_install_record_fallback(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """modinfo + breadcrumb absent → install record is the final source."""
    record = tmp_path / "install-result.json"
    record.write_text(json.dumps({"status": "ok", "wfbModuleSource": "prebuilt"}))
    monkeypatch.setattr(hp, "_wfb_module_source_from_modinfo", lambda: None)
    monkeypatch.setattr(hp, "INSTALL_RESULT", record)
    monkeypatch.setattr(hp, "WFB_MODULE_SOURCE", tmp_path / "wfb-module-source")
    payload = _fresh_app()._build_heartbeat_payload()
    assert payload["wfbModuleSource"] == "prebuilt"


def test_wfb_module_source_modinfo_missing_binary(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """modinfo binary absent (FileNotFoundError) falls through gracefully."""

    def fake_run(*_args, **_kwargs):  # noqa: ANN002, ANN003
        raise FileNotFoundError("modinfo")

    monkeypatch.setattr(hp.subprocess, "run", fake_run)
    monkeypatch.setattr(hp, "INSTALL_RESULT", tmp_path / "install-result.json")
    monkeypatch.setattr(hp, "WFB_MODULE_SOURCE", tmp_path / "wfb-module-source")
    payload = _fresh_app()._build_heartbeat_payload()
    assert payload["wfbModuleSource"] == "none"


def test_wfb_module_source_modinfo_timeout(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """modinfo timing out does not raise; resolution falls through."""

    def fake_run(*_args, **_kwargs):  # noqa: ANN002, ANN003
        raise subprocess.TimeoutExpired(cmd="modinfo", timeout=3.0)

    monkeypatch.setattr(hp.subprocess, "run", fake_run)
    monkeypatch.setattr(hp, "INSTALL_RESULT", tmp_path / "install-result.json")
    monkeypatch.setattr(hp, "WFB_MODULE_SOURCE", tmp_path / "wfb-module-source")
    payload = _fresh_app()._build_heartbeat_payload()
    assert payload["wfbModuleSource"] == "none"


# ---------------------------------------------------------------------------
# installStatus / installVersion / failedSteps
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("status", ["ok", "degraded", "failed"])
def test_install_status_maps_through(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, status: str
) -> None:
    record = tmp_path / "install-result.json"
    record.write_text(
        json.dumps(
            {
                "status": status,
                "version": "1.2.3",
                "failedSteps": ["dkms-build"] if status != "ok" else [],
            }
        )
    )
    monkeypatch.setattr(hp, "_wfb_module_source_from_modinfo", lambda: None)
    monkeypatch.setattr(hp, "INSTALL_RESULT", record)
    monkeypatch.setattr(hp, "WFB_MODULE_SOURCE", tmp_path / "wfb-module-source")
    payload = _fresh_app()._build_heartbeat_payload()
    assert payload["installStatus"] == status
    assert payload["installVersion"] == "1.2.3"
    assert payload["failedSteps"] == (["dkms-build"] if status != "ok" else [])


def test_install_defaults_when_file_absent(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    _no_install_sources(monkeypatch, tmp_path)
    payload = _fresh_app()._build_heartbeat_payload()
    assert payload["installStatus"] == "unknown"
    assert payload["failedSteps"] == []
    assert "installVersion" not in payload


def test_install_garbage_file_does_not_raise(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    record = tmp_path / "install-result.json"
    record.write_text("{ this is not valid json ]")
    monkeypatch.setattr(hp, "_wfb_module_source_from_modinfo", lambda: None)
    monkeypatch.setattr(hp, "INSTALL_RESULT", record)
    monkeypatch.setattr(hp, "WFB_MODULE_SOURCE", tmp_path / "wfb-module-source")
    payload = _fresh_app()._build_heartbeat_payload()
    assert payload["installStatus"] == "unknown"
    assert payload["failedSteps"] == []
    assert payload["wfbModuleSource"] == "none"


def test_install_version_omitted_when_missing(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """Record present but no version key → installVersion omitted."""
    record = tmp_path / "install-result.json"
    record.write_text(json.dumps({"status": "ok"}))
    monkeypatch.setattr(hp, "_wfb_module_source_from_modinfo", lambda: None)
    monkeypatch.setattr(hp, "INSTALL_RESULT", record)
    monkeypatch.setattr(hp, "WFB_MODULE_SOURCE", tmp_path / "wfb-module-source")
    payload = _fresh_app()._build_heartbeat_payload()
    assert payload["installStatus"] == "ok"
    assert "installVersion" not in payload
    assert payload["failedSteps"] == []


def test_install_non_list_failed_steps_coerced(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """A garbage failedSteps value coerces to an empty list, never raises."""
    record = tmp_path / "install-result.json"
    record.write_text(json.dumps({"status": "degraded", "failedSteps": "oops"}))
    monkeypatch.setattr(hp, "_wfb_module_source_from_modinfo", lambda: None)
    monkeypatch.setattr(hp, "INSTALL_RESULT", record)
    monkeypatch.setattr(hp, "WFB_MODULE_SOURCE", tmp_path / "wfb-module-source")
    payload = _fresh_app()._build_heartbeat_payload()
    assert payload["installStatus"] == "degraded"
    assert payload["failedSteps"] == []
