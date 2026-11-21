"""Runtime-aware variant selection, the detector auto-pick, the RKNN platform
filter and the ``benchmark_only`` registry flag of the vision ``ModelManager``."""

from __future__ import annotations

from pathlib import Path
from typing import Any

import pytest

from ados.core.config.system import VisionConfig
from ados.services.vision._model_types import available_runtimes, rknn_platform
from ados.services.vision.model_manager import ModelInfo, ModelManager


def _mgr(
    tmp_path: Path, backend: str, npu_tops: float, board: str | None = None
) -> ModelManager:
    cfg = VisionConfig(models_dir=str(tmp_path), registry_url="", backend=backend)  # type: ignore[arg-type]
    return ModelManager(cfg, npu_tops=npu_tops, board=board)


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


@pytest.mark.parametrize(
    ("board", "want"),
    [
        ("rk3588", "rk3588"),
        ("RK3588S2", "rk3588"),
        ("RK3582", "rk3588"),
        ("Radxa ROCK 5C", "rk3588"),
        ("rock-5c-lite", "rk3588"),
        ("Orange Pi 5", "rk3588"),
        ("RK3576", "rk3576"),
        ("rk3576", "rk3576"),
        ("rk3566", None),
        ("Raspberry Pi 5 Model B", None),
        ("bcm2712", None),
        # /proc/device-tree/compatible entries (NUL-joined there, space-joined
        # in the installer's board string) name the SoC on any carrier.
        ("Radxa ROCK 5B+ radxa,rock-5b-plus rockchip,rk3588", "rk3588"),
        ("Radxa CM4 IO Board radxa,cm4-io radxa,cm4 rockchip,rk3588s", "rk3588"),
        ("Orange Pi 5 rockchip,rk3588s-orangepi-5 rockchip,rk3588", "rk3588"),
        ("Rockchip RK3576 EVB1 V10 Board rockchip,rk3576-evb1-v10 rockchip,rk3576", "rk3576"),
        ("Radxa CM3 IO radxa,cm3-io rockchip,rk3566", None),
        ("Raspberry Pi 5 Model B Rev 1.0 raspberrypi,5-model-b brcm,bcm2712", None),
        ("unknown", None),
        (None, None),
    ],
)
def test_the_board_string_normalises_to_an_rknn_platform(board: str | None, want: str | None) -> None:
    assert rknn_platform(board) == want


def test_auto_offers_the_npu_on_a_board_with_an_rknn_platform() -> None:
    # The installer knows the board, not its TOPS: the platform alone offers rknn.
    assert available_runtimes("auto", 0.0, "rk3588") == ["rknn", "onnx"]
    assert available_runtimes("auto", 0.0, None) == ["onnx"]
    # An explicit backend is never widened.
    assert available_runtimes("rknn", 0.0, "rk3588") == ["rknn"]


def _npu(platform: str, min_tops: float = 2.0) -> dict[str, Any]:
    return {**_variant("rknn", min_tops, f"npu-{platform}"), "platform": platform}


def _detector(*variants: dict[str, Any]) -> list[dict[str, Any]]:
    return [{"id": "det", "task": "detection", "recommended": True, "variants": list(variants)}]


def test_a_rockchip_board_with_no_rknn_file_falls_back_to_the_onnx_detector(tmp_path: Path) -> None:
    # The installer's pick: backend auto, the board string, no TOPS known.
    mgr = _loaded(_mgr(tmp_path, "auto", 0.0, "Radxa ROCK 5C"), _detector(_variant("onnx", 0, "cpu")))
    assert mgr.recommended_model("detection") == "det"
    assert mgr.select_best_variant("det")["runtime"] == "onnx"


def test_a_rockchip_board_takes_the_rknn_file_built_for_its_platform(tmp_path: Path) -> None:
    entries = _detector(_variant("onnx", 0, "cpu"), _npu("rk3588"), _npu("rk3576"))
    rk3588 = _loaded(_mgr(tmp_path, "auto", 0.0, "Radxa ROCK 5C"), entries)
    assert rk3588.select_best_variant("det")["variant"] == "npu-rk3588"
    rk3576 = _loaded(_mgr(tmp_path, "auto", 6.0, "RK3576"), entries)
    assert rk3576.select_best_variant("det")["variant"] == "npu-rk3576"


def test_an_rk3576_board_never_takes_an_rk3588_file(tmp_path: Path) -> None:
    entries = _detector(_variant("onnx", 0, "cpu"), _npu("rk3588"))
    # auto: the rk3588 file is skipped and the ONNX variant wins.
    auto = _loaded(_mgr(tmp_path, "auto", 6.0, "RK3576"), entries)
    assert auto.select_best_variant("det")["variant"] == "cpu"
    # An explicit rknn backend has nothing it can load.
    forced = _loaded(_mgr(tmp_path, "rknn", 6.0, "rk3576"), entries)
    assert forced.select_best_variant("det") is None
    assert forced.recommended_model("detection") is None
    # And a board with no platform never takes a platform-built file either.
    unknown = _loaded(_mgr(tmp_path, "rknn", 6.0, None), entries)
    assert unknown.select_best_variant("det") is None


def test_a_board_without_an_npu_takes_the_onnx_detector(tmp_path: Path) -> None:
    entries = _detector(_variant("onnx", 0, "cpu"), _npu("rk3588"))
    pi = _loaded(_mgr(tmp_path, "auto", 0.0, "Raspberry Pi 5 Model B"), entries)
    assert pi.select_best_variant("det")["variant"] == "cpu"


def _person_registry(rknn_platforms: tuple[str, ...]) -> list[dict[str, Any]]:
    """The shape the public registry ships: Tiny is the CPU default, S the NPU
    default (and an ONNX variant any board may pick explicitly)."""
    return [
        {
            "id": "tiny",
            "task": "detection",
            "recommended_for": ["onnx"],
            "variants": [_variant("onnx", 0, "tiny-cpu")],
        },
        {
            "id": "s",
            "task": "detection",
            "recommended_for": ["rknn"],
            "variants": [_variant("onnx", 0, "s-cpu"), *(_npu(p) for p in rknn_platforms)],
        },
    ]


def test_a_cpu_board_defaults_to_the_real_time_detector(tmp_path: Path) -> None:
    pi = _loaded(_mgr(tmp_path, "auto", 0.0, "Raspberry Pi 5 Model B"), _person_registry(("rk3588",)))
    assert pi.recommended_model("detection") == "tiny"
    assert pi.select_best_variant("tiny")["runtime"] == "onnx"
    # The larger model stays installable on the CPU.
    assert pi.select_best_variant("s")["variant"] == "s-cpu"


def test_an_npu_board_with_a_platform_file_defaults_to_the_npu_detector(tmp_path: Path) -> None:
    rk = _loaded(_mgr(tmp_path, "auto", 0.0, "rockchip,rk3588"), _person_registry(("rk3588",)))
    assert rk.recommended_model("detection") == "s"
    assert rk.select_best_variant("s")["variant"] == "npu-rk3588"


def test_an_npu_board_without_a_platform_file_falls_back_to_the_real_time_cpu_detector(
    tmp_path: Path,
) -> None:
    # No rk3588 file: S would run on the CPU, so the CPU default wins.
    rk = _loaded(_mgr(tmp_path, "auto", 0.0, "rockchip,rk3588"), _person_registry(("rk3576",)))
    assert rk.recommended_model("detection") == "tiny"
    assert rk.select_best_variant("tiny")["runtime"] == "onnx"
