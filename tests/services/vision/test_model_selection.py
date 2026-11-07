"""Runtime-aware variant selection, the detector auto-pick and the
``benchmark_only`` registry flag of the vision ``ModelManager``."""

from __future__ import annotations

from pathlib import Path
from typing import Any

import pytest

from ados.core.config.system import VisionConfig
from ados.services.vision._model_types import available_runtimes
from ados.services.vision.model_manager import ModelInfo, ModelManager


def _mgr(tmp_path: Path, backend: str, npu_tops: float) -> ModelManager:
    cfg = VisionConfig(models_dir=str(tmp_path), registry_url="", backend=backend)  # type: ignore[arg-type]
    return ModelManager(cfg, npu_tops=npu_tops)


def _variant(runtime: str, min_tops: float, name: str) -> dict[str, Any]:
    return {
        "variant": name,
        "runtime": runtime,
        "min_tops": min_tops,
        "filename": f"{name}.{runtime}",
        "url": f"https://example.invalid/{name}.{runtime}",
    }


def _registry() -> list[dict[str, Any]]:
    return [
        # A placeholder: no runtime, no URL. Never auto-picked.
        {"id": "placeholder", "task": "detection", "variants": [{"variant": "nano", "min_tops": 0}]},
        {
            "id": "npu-det",
            "task": "detection",
            "variants": [_variant("rknn", 1.0, "int8"), _variant("rknn", 2.0, "fp16")],
        },
        {"id": "cpu-det", "task": "detection", "variants": [_variant("onnx", 0, "fp32")]},
        {
            "id": "both-det",
            "task": "detection",
            "variants": [_variant("onnx", 0, "cpu"), _variant("rknn", 1.0, "npu")],
        },
        {
            "id": "reid-bench",
            "task": "reid",
            "benchmark_only": True,
            "variants": [_variant("onnx", 0, "reid")],
        },
    ]


def _loaded(mgr: ModelManager, entries: list[dict[str, Any]]) -> ModelManager:
    mgr._registry = [ModelInfo.from_registry(e) for e in entries]
    return mgr


@pytest.mark.parametrize(
    ("backend", "tops", "want"),
    [
        ("auto", 6.0, ["rknn", "onnx"]),
        ("auto", 0.0, ["onnx"]),
        ("onnx", 6.0, ["onnx"]),
        ("rknn", 0.0, ["rknn"]),
        ("hailo", 13.0, ["hailo"]),
        ("mock", 0.0, None),
    ],
)
def test_available_runtimes_follow_the_backend(
    backend: str, tops: float, want: list[str] | None
) -> None:
    assert available_runtimes(backend, tops) == want


def test_runtime_wins_over_tops(tmp_path: Path) -> None:
    # An NPU board prefers the NPU variant; a CPU board takes the ONNX one even
    # though it is listed first and the NPU one "fits" nothing there.
    npu = _loaded(_mgr(tmp_path, "auto", 6.0), _registry())
    assert npu.select_best_variant("both-det")["variant"] == "npu"
    cpu = _loaded(_mgr(tmp_path, "auto", 0.0), _registry())
    assert cpu.select_best_variant("both-det")["variant"] == "cpu"
    # An explicit onnx backend on an NPU board still runs ONNX.
    forced = _loaded(_mgr(tmp_path, "onnx", 6.0), _registry())
    assert forced.select_best_variant("both-det")["variant"] == "cpu"


def test_tops_break_the_tie_within_a_runtime(tmp_path: Path) -> None:
    assert _loaded(_mgr(tmp_path, "rknn", 6.0), _registry()).select_best_variant(
        "npu-det"
    )["variant"] == "fp16"
    assert _loaded(_mgr(tmp_path, "rknn", 1.5), _registry()).select_best_variant(
        "npu-det"
    )["variant"] == "int8"
    # Nothing fits: the least demanding variant of the runnable runtime.
    assert _loaded(_mgr(tmp_path, "rknn", 0.0), _registry()).select_best_variant(
        "npu-det"
    )["variant"] == "int8"


def test_a_model_with_no_runnable_variant_has_none(tmp_path: Path) -> None:
    cpu = _loaded(_mgr(tmp_path, "auto", 0.0), _registry())
    assert cpu.select_best_variant("npu-det") is None
    npu = _loaded(_mgr(tmp_path, "rknn", 6.0), _registry())
    assert npu.select_best_variant("cpu-det") is None


def test_recommended_detector_is_runnable_and_downloadable(tmp_path: Path) -> None:
    # The placeholder (no URL) is skipped on every board.
    npu = _loaded(_mgr(tmp_path, "rknn", 6.0), _registry())
    assert npu.recommended_model("detection") == "npu-det"
    cpu = _loaded(_mgr(tmp_path, "auto", 0.0), _registry())
    assert cpu.recommended_model("detection") == "cpu-det"
    # A recommended flag wins among the runnable candidates.
    entries = _registry()
    entries[3]["recommended"] = True
    assert _loaded(_mgr(tmp_path, "auto", 0.0), entries).recommended_model("detection") == "both-det"
    # Nothing runnable: no pick.
    hailo = _loaded(_mgr(tmp_path, "hailo", 13.0), _registry())
    assert hailo.recommended_model("detection") is None


def test_the_depth_pick_is_its_own_task_and_follows_the_runtime(tmp_path: Path) -> None:
    entries = _registry()
    entries.append(
        {
            "id": "depth-small",
            "task": "depth",
            "recommended": True,
            "variants": [_variant("onnx", 0, "depth-cpu"), _variant("rknn", 1.0, "depth-npu")],
        }
    )
    cpu = _loaded(_mgr(tmp_path, "auto", 0.0), entries)
    assert cpu.recommended_model("depth") == "depth-small"
    assert cpu.select_best_variant("depth-small")["variant"] == "depth-cpu"
    npu = _loaded(_mgr(tmp_path, "rknn", 6.0), entries)
    assert npu.select_best_variant("depth-small")["variant"] == "depth-npu"
    # A depth model is never the detector pick.
    assert cpu.recommended_model("detection") == "cpu-det"
    # The hailo runtime has no depth variant.
    assert _loaded(_mgr(tmp_path, "hailo", 13.0), entries).recommended_model("depth") is None


def test_an_installed_file_carries_the_task_of_the_registry_model_that_names_it(
    tmp_path: Path,
) -> None:
    entries = _registry()
    entries.append(
        {"id": "depth-small", "task": "depth", "variants": [_variant("onnx", 0, "depth-cpu")]}
    )
    mgr = _loaded(_mgr(tmp_path, "auto", 0.0), entries)
    (tmp_path / "depth-cpu.onnx").write_bytes(b"x")
    (tmp_path / "fp32.onnx").write_bytes(b"x")
    (tmp_path / "sideloaded.onnx").write_bytes(b"x")
    by_id = {m["id"]: m for m in mgr.list_installed()}
    assert (by_id["depth-cpu"]["task"], by_id["depth-cpu"]["registry_id"]) == ("depth", "depth-small")
    assert by_id["fp32"]["task"] == "detection"
    assert "task" not in by_id["sideloaded"]


def test_a_benchmark_only_model_is_never_auto_picked_but_stays_installable(tmp_path: Path) -> None:
    entries = _registry()
    entries.append(
        {
            "id": "bench-det",
            "task": "detection",
            "benchmark_only": True,
            "recommended": True,
            "variants": [_variant("onnx", 0, "bench")],
        }
    )
    mgr = _loaded(_mgr(tmp_path, "auto", 0.0), entries)
    info = next(m for m in mgr.registry if m.id == "bench-det")
    assert info.benchmark_only is True
    assert info.to_dict()["benchmark_only"] is True
    assert mgr.recommended_model("detection") == "cpu-det"
    # An operator can still install it explicitly: a variant resolves.
    assert mgr.select_best_variant("bench-det")["variant"] == "bench"
    assert mgr.select_best_variant("reid-bench")["variant"] == "reid"
