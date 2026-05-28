"""Tests for the vision ``ModelManager``.

Covers manifest parsing (cache + remote), registry HTTP fetch with ETag
caching, installed-model enumeration, variant selection by NPU TOPS,
cache-usage accounting against ``models_cache_max_mb``, and the
``auto_download`` / registry-disabled paths.

External I/O is mocked at the ``httpx.AsyncClient`` layer so no real
network or DNS happens. File system writes are scoped to ``tmp_path``.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any
from unittest.mock import MagicMock, patch

import httpx
import pytest

from ados.core.config.system import VisionConfig
from ados.services.vision.model_manager import (
    DownloadProgress,
    DownloadState,
    ModelInfo,
    ModelManager,
)

# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


def _make_config(tmp_path: Path, **overrides: Any) -> VisionConfig:
    """Build a VisionConfig pointing at an isolated models_dir."""
    base: dict[str, Any] = {
        "models_dir": str(tmp_path / "models"),
        "models_cache_max_mb": 10,
        "registry_url": "https://example.invalid/registry.json",
    }
    base.update(overrides)
    return VisionConfig(**base)


def _sample_registry() -> list[dict[str, Any]]:
    return [
        {
            "id": "yolov8n",
            "name": "YOLOv8 Nano",
            "description": "Lightweight detector",
            "task": "detection",
            "variants": [
                {
                    "min_tops": 0.5,
                    "url": "https://example.invalid/yolov8n.rknn",
                    "filename": "yolov8n.rknn",
                    "size_bytes": 1024,
                },
                {
                    "min_tops": 6.0,
                    "url": "https://example.invalid/yolov8n-fp16.rknn",
                    "filename": "yolov8n-fp16.rknn",
                    "size_bytes": 2048,
                },
            ],
        },
        {
            "id": "tracker-lite",
            "name": "Tracker Lite",
            "task": "tracking",
            "variants": [],
        },
    ]


class _FakeAsyncResponse:
    """Drop-in async response that mimics the httpx surface we use."""

    def __init__(
        self,
        status_code: int = 200,
        headers: dict[str, str] | None = None,
        payload: Any = None,
    ) -> None:
        self.status_code = status_code
        self.headers = headers or {}
        self._payload = payload

    def raise_for_status(self) -> None:
        if self.status_code >= 400:
            raise httpx.HTTPStatusError(
                "boom",
                request=MagicMock(),
                response=MagicMock(status_code=self.status_code),
            )

    def json(self) -> Any:
        return self._payload


class _FakeAsyncClient:
    """Async-context client that returns a queued response on get()."""

    def __init__(self, response: _FakeAsyncResponse | Exception) -> None:
        self._response = response
        self.calls: list[tuple[str, dict[str, str]]] = []

    async def __aenter__(self) -> _FakeAsyncClient:
        return self

    async def __aexit__(self, *_exc: Any) -> None:
        return None

    async def get(self, url: str, headers: dict[str, str] | None = None) -> _FakeAsyncResponse:
        self.calls.append((url, dict(headers or {})))
        if isinstance(self._response, Exception):
            raise self._response
        return self._response


# ---------------------------------------------------------------------------
# Construction + cached registry loading
# ---------------------------------------------------------------------------


def test_init_without_cache_yields_empty_registry(tmp_path: Path) -> None:
    """Fresh install: no etag, no cache, empty in-memory registry."""
    config = _make_config(tmp_path)
    mgr = ModelManager(config, npu_tops=1.0)
    assert mgr.registry == []
    assert mgr._etag == ""


def test_init_loads_cached_registry_from_disk(tmp_path: Path) -> None:
    """A pre-existing ``.registry_cache.json`` populates the registry."""
    models_dir = tmp_path / "models"
    models_dir.mkdir()
    cache_path = models_dir / ".registry_cache.json"
    cache_path.write_text(json.dumps(_sample_registry()))

    config = _make_config(tmp_path)
    mgr = ModelManager(config, npu_tops=1.0)

    assert len(mgr.registry) == 2
    assert mgr.registry[0].id == "yolov8n"
    assert mgr.registry[0].task == "detection"


def test_init_handles_corrupt_cache_gracefully(tmp_path: Path) -> None:
    """Corrupt JSON in the cache must not crash construction."""
    models_dir = tmp_path / "models"
    models_dir.mkdir()
    (models_dir / ".registry_cache.json").write_text("{ not valid json")

    config = _make_config(tmp_path)
    mgr = ModelManager(config, npu_tops=1.0)
    assert mgr.registry == []


def test_init_loads_cache_with_models_key_wrapper(tmp_path: Path) -> None:
    """Registry may arrive either as a list or as ``{"models": [...]}``."""
    models_dir = tmp_path / "models"
    models_dir.mkdir()
    cache_path = models_dir / ".registry_cache.json"
    cache_path.write_text(json.dumps({"models": _sample_registry()}))

    mgr = ModelManager(_make_config(tmp_path), npu_tops=1.0)
    assert len(mgr.registry) == 2


def test_init_tolerates_missing_required_fields(tmp_path: Path) -> None:
    """Entries with missing fields fall back to safe defaults."""
    models_dir = tmp_path / "models"
    models_dir.mkdir()
    (models_dir / ".registry_cache.json").write_text(
        json.dumps([{"name": "no-id"}, {}])
    )

    mgr = ModelManager(_make_config(tmp_path), npu_tops=1.0)
    assert len(mgr.registry) == 2
    assert mgr.registry[0].id == ""
    assert mgr.registry[0].name == "no-id"
    assert mgr.registry[1].variants == []


def test_init_reads_persisted_etag(tmp_path: Path) -> None:
    models_dir = tmp_path / "models"
    models_dir.mkdir()
    (models_dir / ".registry_etag").write_text('W/"abc123"\n')

    mgr = ModelManager(_make_config(tmp_path), npu_tops=1.0)
    assert mgr._etag == 'W/"abc123"'


# ---------------------------------------------------------------------------
# Remote registry fetch
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_fetch_registry_no_url_returns_cached(tmp_path: Path) -> None:
    """Empty ``registry_url`` returns the in-memory registry without HTTP."""
    config = _make_config(tmp_path, registry_url="")
    mgr = ModelManager(config, npu_tops=1.0)
    mgr._registry = [ModelInfo(id="seed")]

    result = await mgr.fetch_registry()
    assert [m.id for m in result] == ["seed"]


@pytest.mark.asyncio
async def test_fetch_registry_happy_path(tmp_path: Path) -> None:
    """Status 200 populates the registry and writes the cache + etag."""
    response = _FakeAsyncResponse(
        status_code=200,
        headers={"ETag": 'W/"v1"'},
        payload=_sample_registry(),
    )
    client = _FakeAsyncClient(response)

    config = _make_config(tmp_path)
    mgr = ModelManager(config, npu_tops=1.0)

    with patch("httpx.AsyncClient", return_value=client):
        models = await mgr.fetch_registry()

    assert len(models) == 2
    assert models[0].id == "yolov8n"
    # ETag persisted and cache written
    assert (tmp_path / "models" / ".registry_etag").read_text() == 'W/"v1"'
    assert (tmp_path / "models" / ".registry_cache.json").exists()
    # No If-None-Match header on first call (no prior etag)
    assert "If-None-Match" not in client.calls[0][1]


@pytest.mark.asyncio
async def test_fetch_registry_304_keeps_existing_registry(tmp_path: Path) -> None:
    """A 304 Not Modified response leaves the cached registry intact."""
    seeded = [ModelInfo(id="cached")]
    response = _FakeAsyncResponse(status_code=304)
    client = _FakeAsyncClient(response)

    config = _make_config(tmp_path)
    mgr = ModelManager(config, npu_tops=1.0)
    mgr._registry = seeded
    mgr._etag = 'W/"prev"'

    with patch("httpx.AsyncClient", return_value=client):
        models = await mgr.fetch_registry()

    assert [m.id for m in models] == ["cached"]
    # The cached etag flows back out as the If-None-Match header.
    assert client.calls[0][1].get("If-None-Match") == 'W/"prev"'


@pytest.mark.asyncio
async def test_fetch_registry_http_error_returns_cached(tmp_path: Path) -> None:
    """HTTP errors are swallowed; the prior registry survives."""
    seeded = [ModelInfo(id="cached")]
    client = _FakeAsyncClient(httpx.ConnectError("dns down"))

    config = _make_config(tmp_path)
    mgr = ModelManager(config, npu_tops=1.0)
    mgr._registry = seeded

    with patch("httpx.AsyncClient", return_value=client):
        models = await mgr.fetch_registry()

    assert [m.id for m in models] == ["cached"]


@pytest.mark.asyncio
async def test_fetch_registry_handles_models_key_wrapper(tmp_path: Path) -> None:
    """``{"models": [...]}`` envelope is accepted on the wire too."""
    response = _FakeAsyncResponse(
        status_code=200,
        headers={},
        payload={"models": _sample_registry()},
    )
    client = _FakeAsyncClient(response)

    config = _make_config(tmp_path)
    mgr = ModelManager(config, npu_tops=1.0)

    with patch("httpx.AsyncClient", return_value=client):
        models = await mgr.fetch_registry()

    assert [m.id for m in models] == ["yolov8n", "tracker-lite"]


# ---------------------------------------------------------------------------
# Installed-model enumeration
# ---------------------------------------------------------------------------


def test_list_installed_empty_when_dir_missing(tmp_path: Path) -> None:
    config = _make_config(tmp_path)
    mgr = ModelManager(config, npu_tops=1.0)
    assert mgr.list_installed() == []


def test_list_installed_filters_by_extension(tmp_path: Path) -> None:
    models_dir = tmp_path / "models"
    models_dir.mkdir()
    (models_dir / "a.rknn").write_bytes(b"x" * 32)
    (models_dir / "b.tflite").write_bytes(b"x" * 64)
    (models_dir / "c.onnx").write_bytes(b"x" * 16)
    (models_dir / "d.engine").write_bytes(b"x" * 8)
    (models_dir / "ignore.txt").write_bytes(b"nope")
    (models_dir / ".registry_etag").write_text("etag")

    mgr = ModelManager(_make_config(tmp_path), npu_tops=1.0)
    installed = mgr.list_installed()

    formats = sorted(item["format"] for item in installed)
    assert formats == ["engine", "onnx", "rknn", "tflite"]
    ids = sorted(item["id"] for item in installed)
    assert ids == ["a", "b", "c", "d"]


# ---------------------------------------------------------------------------
# Variant selection
# ---------------------------------------------------------------------------


def test_select_best_variant_picks_highest_fitting_tops(tmp_path: Path) -> None:
    """When 6 TOPS is available the FP16 variant must win."""
    mgr = ModelManager(_make_config(tmp_path), npu_tops=6.0)
    mgr._registry = [ModelInfo(**entry) for entry in _sample_registry()]

    variant = mgr.select_best_variant("yolov8n")
    assert variant is not None
    assert variant["filename"] == "yolov8n-fp16.rknn"


def test_select_best_variant_falls_back_to_lowest(tmp_path: Path) -> None:
    """No variant fits the board? Return the lowest-requirement one."""
    mgr = ModelManager(_make_config(tmp_path), npu_tops=0.1)
    mgr._registry = [ModelInfo(**entry) for entry in _sample_registry()]

    variant = mgr.select_best_variant("yolov8n")
    assert variant is not None
    assert variant["min_tops"] == 0.5


def test_select_best_variant_unknown_model_returns_none(tmp_path: Path) -> None:
    mgr = ModelManager(_make_config(tmp_path), npu_tops=10.0)
    mgr._registry = [ModelInfo(**entry) for entry in _sample_registry()]

    assert mgr.select_best_variant("does-not-exist") is None


def test_select_best_variant_model_without_variants_returns_none(tmp_path: Path) -> None:
    """``tracker-lite`` in the sample has an empty variants list."""
    mgr = ModelManager(_make_config(tmp_path), npu_tops=10.0)
    mgr._registry = [ModelInfo(**entry) for entry in _sample_registry()]

    assert mgr.select_best_variant("tracker-lite") is None


# ---------------------------------------------------------------------------
# Download contract — failure paths only (happy path runs streaming HTTP)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_download_model_rejects_unknown_model(tmp_path: Path) -> None:
    mgr = ModelManager(_make_config(tmp_path), npu_tops=10.0)
    mgr._registry = []
    with pytest.raises(ValueError, match="No suitable variant"):
        await mgr.download_model("ghost")


@pytest.mark.asyncio
async def test_download_model_rejects_missing_url(tmp_path: Path) -> None:
    """A variant with an empty download URL is unusable."""
    registry = [
        {
            "id": "broken",
            "task": "detection",
            "variants": [
                {"min_tops": 0.0, "url": "", "filename": "broken.rknn", "size_bytes": 1}
            ],
        }
    ]
    mgr = ModelManager(_make_config(tmp_path), npu_tops=1.0)
    mgr._registry = [ModelInfo(**entry) for entry in registry]
    with pytest.raises(ValueError, match="No download URL"):
        await mgr.download_model("broken")


# ---------------------------------------------------------------------------
# Progress + cache accounting
# ---------------------------------------------------------------------------


def test_download_progress_percent() -> None:
    p = DownloadProgress(total_bytes=100, bytes_downloaded=25)
    assert p.percent() == 25.0
    p.bytes_downloaded = 500
    assert p.percent() == 100.0  # clamped


def test_download_progress_percent_when_total_zero() -> None:
    p = DownloadProgress(total_bytes=0, bytes_downloaded=99)
    assert p.percent() == 0.0


def test_download_progress_to_dict_shape() -> None:
    p = DownloadProgress(
        state=DownloadState.DOWNLOADING,
        bytes_downloaded=512,
        total_bytes=1024,
        speed_bps=200.0,
        eta_seconds=2.0,
    )
    snapshot = p.to_dict()
    assert snapshot["state"] == "downloading"
    assert snapshot["percent"] == 50.0
    assert snapshot["bytes_downloaded"] == 512
    assert snapshot["total_bytes"] == 1024


def test_get_download_progress_default_when_absent(tmp_path: Path) -> None:
    mgr = ModelManager(_make_config(tmp_path), npu_tops=1.0)
    snap = mgr.get_download_progress("unknown")
    assert snap.state == DownloadState.IDLE


def test_get_cache_usage_reports_zero_when_empty(tmp_path: Path) -> None:
    mgr = ModelManager(_make_config(tmp_path, models_cache_max_mb=42), npu_tops=1.0)
    usage = mgr.get_cache_usage()
    assert usage["used_bytes"] == 0
    assert usage["max_mb"] == 42
    assert usage["max_bytes"] == 42 * 1024 * 1024


def test_get_cache_usage_counts_only_model_extensions(tmp_path: Path) -> None:
    """Non-model files in the directory must not inflate cache usage."""
    models_dir = tmp_path / "models"
    models_dir.mkdir()
    (models_dir / "a.rknn").write_bytes(b"x" * 100)
    (models_dir / "b.tflite").write_bytes(b"x" * 200)
    (models_dir / "not-a-model.bin").write_bytes(b"x" * 99999)
    (models_dir / ".registry_etag").write_text("etag")

    mgr = ModelManager(_make_config(tmp_path, models_cache_max_mb=1), npu_tops=1.0)
    usage = mgr.get_cache_usage()
    assert usage["used_bytes"] == 300
    assert usage["max_bytes"] == 1024 * 1024


# ---------------------------------------------------------------------------
# Dataclass helpers
# ---------------------------------------------------------------------------


def test_model_info_to_dict_round_trip() -> None:
    info = ModelInfo(
        id="m1",
        name="Model One",
        description="desc",
        task="detection",
        variants=[{"min_tops": 1.0}],
    )
    payload = info.to_dict()
    assert payload == {
        "id": "m1",
        "name": "Model One",
        "description": "desc",
        "task": "detection",
        "variants": [{"min_tops": 1.0}],
    }
