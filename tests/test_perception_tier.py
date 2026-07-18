"""Perception tier decision + the board NPU-capability signal (W3 of the
perception program). The tier mirror is validated branch-for-branch against
the canonical ados-offload::pick_tier contract."""

from __future__ import annotations

from ados.hal.detect import BoardInfo
from ados.services.vision.tier import perception_tier


class TestPerceptionTier:
    def test_accelerator_that_fits_is_local(self) -> None:
        # An NPU board runs detection locally even when a node is paired.
        assert (
            perception_tier(
                has_accelerator=True,
                compute_node_paired=True,
                bearer_acceptable=True,
            )
            == "local"
        )

    def test_no_accelerator_with_a_paired_node_offloads(self) -> None:
        assert (
            perception_tier(
                has_accelerator=False,
                compute_node_paired=True,
                bearer_acceptable=True,
            )
            == "offload"
        )

    def test_a_light_local_board_with_a_node_is_hybrid(self) -> None:
        assert (
            perception_tier(
                has_accelerator=False,
                compute_node_paired=True,
                bearer_acceptable=True,
                can_run_light_local=True,
            )
            == "hybrid"
        )

    def test_no_accelerator_no_node_is_none(self) -> None:
        # Bare odometry only: no detection / tracking / map-based autonomy.
        assert perception_tier(has_accelerator=False) == "none"

    def test_an_unacceptable_bearer_is_not_an_offload_path(self) -> None:
        assert (
            perception_tier(
                has_accelerator=False,
                compute_node_paired=True,
                bearer_acceptable=False,
            )
            == "none"
        )

    def test_the_default_inputs_reduce_to_accelerator_only(self) -> None:
        # The shipped signal today: an NPU board is local, everything else none
        # (the offload signals default False until the orchestration wires them).
        assert perception_tier(has_accelerator=True) == "local"
        assert perception_tier(has_accelerator=False) == "none"

    def test_cpu_onnx_capable_board_is_local_without_an_accelerator(self) -> None:
        # A board with no NPU but a CPU strong enough for the ONNX detector runs
        # detection on-board (a full local path), local even with a node paired.
        assert (
            perception_tier(has_accelerator=False, local_inference_capable=True)
            == "local"
        )
        assert (
            perception_tier(
                has_accelerator=False,
                local_inference_capable=True,
                compute_node_paired=True,
                bearer_acceptable=True,
            )
            == "local"
        )


class TestBoardAccelerator:
    def test_npu_board_reports_accelerator(self) -> None:
        b = BoardInfo(
            name="rk", model="RK3588", tier=4, ram_mb=8000, cpu_cores=8, npu_tops=6.0
        )
        assert b.has_accelerator is True
        d = b.to_dict()
        assert d["npu_tops"] == 6.0
        assert d["has_accelerator"] is True

    def test_cpu_board_reports_no_accelerator(self) -> None:
        b = BoardInfo(name="pi", model="Raspberry Pi 4", tier=3, ram_mb=4000, cpu_cores=4)
        assert b.has_accelerator is False
        d = b.to_dict()
        assert d["npu_tops"] == 0.0
        assert d["has_accelerator"] is False
        # Default board: no CPU-inference declaration.
        assert d["local_inference"] == "none"
        assert d["has_local_inference"] is False

    def test_cpu_onnx_board_reports_local_inference(self) -> None:
        # A CPU-strong board that declares ONNX local inference reports it in the
        # sidecar dict (a full local path) while still having no NPU.
        b = BoardInfo(
            name="pi5",
            model="Raspberry Pi 5",
            tier=3,
            ram_mb=8000,
            cpu_cores=4,
            local_inference="onnx",
        )
        assert b.has_accelerator is False
        assert b.has_local_inference is True
        d = b.to_dict()
        assert d["local_inference"] == "onnx"
        assert d["has_local_inference"] is True
        assert d["has_accelerator"] is False
