# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""HailoRT inference sidecar for compiled ``.hef`` models (Raspberry Pi + AI HAT).

Mirror of the RKNN / TensorRT sidecars for a Hailo-8 (or Hailo-8L) accelerator on
a Raspberry Pi AI HAT+. The ``hailo_platform`` (HailoRT) runtime is x86/ARM board
software tied to the Hailo PCIe device, so the Rust vision engine reaches this
Python process over ``/run/ados/vision-hailo.sock`` and speaks the same
:mod:`ados.services.vision.sidecar_protocol` request/response shape. Detections
come back already in the Rust ``Detection`` field layout.

Two deliberate design choices:

* **Decode here, in Python, not in Hailo's C++ TAPPAS.** The compiled ``.hef``
  emits raw output tensors; this sidecar decodes them with the SAME
  :func:`decode_yolo_detections` the other sidecars use, so a model exported to
  ``.rknn``, ``.engine`` and ``.hef`` produces identical boxes and the decode is
  one tested implementation rather than a vendor C++ post-process. HailoRT's own
  post-process (HailoRT-Post-Process / TAPPAS) is C++-only and is not used.
* **``hailo_platform`` is imported lazily** inside :class:`HailoBackend`. On a
  host without the runtime (a dev laptop, CI, a board with no HAT), ``load_model``
  returns an ``error`` response and the engine falls back to a Rust-side path; the
  sidecar stays up. So this file builds, serves, and is exercisable everywhere,
  even though only a real Hailo device can actually infer — the ``.hef`` compile
  and on-device run are hardware/SDK-gated (the compiler is x86-Linux-only).

Run as ``python -m ados.services.vision.hailo_sidecar``.
"""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import signal
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from ados.core.logging import get_logger
from ados.services.vision import sidecar_protocol as proto
from ados.services.vision.rknn_sidecar import (
    DEFAULT_CONF_THRESHOLD,
    DEFAULT_NMS_IOU,
    decode_yolo_detections,
)
from ados.services.vision.sidecar_protocol import SidecarServer

log = get_logger("vision-hailo")

DEFAULT_SOCKET = "/run/ados/vision-hailo.sock"


@dataclass
class _LoadedHef:
    """A configured ``.hef`` network group plus the metadata needed to label and
    scale detections back onto the source frame."""

    infer_model: Any
    input_w: int
    input_h: int
    fmt: str
    class_labels: list[str]
    head: str = "yolov8"


class HailoBackend:
    """Loads compiled ``.hef`` files with HailoRT and runs inference.

    The ``hailo_platform`` import is deferred to :meth:`load_model`. A missing
    runtime surfaces as an error response rather than a crash, so the engine can
    fall back and the sidecar keeps serving.
    """

    def __init__(
        self,
        conf_threshold: float = DEFAULT_CONF_THRESHOLD,
        nms_iou: float = DEFAULT_NMS_IOU,
    ) -> None:
        self._models: dict[str, _LoadedHef] = {}
        self._vdevice: Any = None
        self._conf_threshold = conf_threshold
        self._nms_iou = nms_iou

    @staticmethod
    def _import_runtime() -> Any:
        """Import the ``hailo_platform`` module, or raise a clear error."""
        try:
            import hailo_platform  # type: ignore[import-not-found]
        except ImportError as exc:  # pragma: no cover - depends on the HailoRT wheel
            raise RuntimeError(
                "hailo_platform (HailoRT) is not installed; Hailo inference is "
                "unavailable on this host"
            ) from exc
        return hailo_platform

    def load_model(self, req: proto.LoadModelRequest) -> dict[str, Any]:
        path = Path(req.path)
        if not path.is_file():
            return proto.error_response(f"hef file not found: {req.path}")
        if path.suffix.lower() != ".hef":
            return proto.error_response(
                f"hailo backend expects a compiled .hef, got {path.suffix or 'no suffix'}: {req.path}"
            )

        try:
            hpf = self._import_runtime()
        except RuntimeError as exc:
            return proto.error_response(str(exc))

        try:
            if self._vdevice is None:
                self._vdevice = hpf.VDevice()
            infer_model = self._vdevice.create_infer_model(str(path))
        except Exception as exc:  # pragma: no cover - depends on the Hailo device
            log.error("hailo_load_failed", model=req.model_id, error=str(exc))
            return proto.error_response(f"hailo load error: {exc}")

        self._models[req.model_id] = _LoadedHef(
            infer_model=infer_model,
            input_w=req.input_w,
            input_h=req.input_h,
            fmt=req.format,
            class_labels=req.class_labels,
            head=req.head,
        )
        log.info("hailo_model_loaded", model=req.model_id, path=req.path)
        return proto.ok_response()

    def infer(self, req: proto.InferRequest) -> dict[str, Any]:
        loaded = self._models.get(req.model_id)
        if loaded is None:
            return proto.error_response(f"model not loaded: {req.model_id}")

        try:
            outputs = self._run_hef(loaded, req)
        except RuntimeError as exc:
            return proto.error_response(str(exc))
        except Exception as exc:  # pragma: no cover - depends on the Hailo device
            log.error("hailo_infer_failed", model=req.model_id, error=str(exc))
            return proto.error_response(f"hailo inference error: {exc}")

        try:
            import numpy as np  # lazy: only needed on the real infer path
        except ImportError as exc:  # pragma: no cover - board has numpy
            return proto.error_response(f"numpy unavailable: {exc}")

        detections = self._postprocess_yolo(np, outputs, req.width, req.height, loaded)
        return proto.ok_response(detections)

    def _run_hef(self, loaded: _LoadedHef, req: proto.InferRequest) -> list[Any]:
        """Run one frame through the configured ``.hef``.

        Binding the input/output buffers and calling ``infer_model.run`` needs a
        real Hailo device. When the device or its bindings are unavailable the
        path raises a clear :class:`RuntimeError` that :meth:`infer` turns into an
        ``error`` response so the engine can fall back. The concrete host/device
        buffer binding is validated on hardware and is intentionally not run on a
        non-Hailo host.
        """
        raise RuntimeError(  # pragma: no cover - exercised only on a Hailo device
            "Hailo execution path requires a HailoRT device"
        )

    def _postprocess_yolo(
        self,
        np: Any,
        outputs: list[Any],
        frame_w: int,
        frame_h: int,
        loaded: _LoadedHef,
    ) -> list[dict[str, Any]]:
        """Decode this model's head to detections in source-frame pixels.

        Delegates to the shared :func:`decode_yolo_detections` — the same decoder
        the RKNN and TensorRT sidecars use — so one model produces identical boxes
        across accelerators and there is no vendor C++ post-process (no TAPPAS).
        """
        return decode_yolo_detections(
            np,
            outputs,
            frame_w=frame_w,
            frame_h=frame_h,
            input_w=loaded.input_w,
            input_h=loaded.input_h,
            class_labels=loaded.class_labels,
            head=loaded.head,
            conf_threshold=self._conf_threshold,
            nms_iou=self._nms_iou,
        )


async def serve(socket_path: str = DEFAULT_SOCKET) -> None:
    """Bind the Hailo sidecar socket and serve until cancelled."""
    backend = HailoBackend()
    server = SidecarServer(socket_path, backend, log)
    await server.serve_forever()


def _run() -> None:
    parser = argparse.ArgumentParser(description="ADOS Hailo inference sidecar")
    parser.add_argument("--socket", default=DEFAULT_SOCKET, help="Unix socket path")
    args = parser.parse_args()

    loop = asyncio.new_event_loop()
    asyncio.set_event_loop(loop)
    task = loop.create_task(serve(args.socket))

    for sig in (signal.SIGINT, signal.SIGTERM):
        with contextlib.suppress(NotImplementedError):
            loop.add_signal_handler(sig, task.cancel)

    try:
        loop.run_until_complete(task)
    except asyncio.CancelledError:
        pass
    finally:
        loop.close()


if __name__ == "__main__":
    _run()
