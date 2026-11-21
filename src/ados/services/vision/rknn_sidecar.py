# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""NPU inference sidecar for RKNN models (RK3588/RK3582/RK3576).

The Rust vision engine cannot link rknn-toolkit-lite2: it ships as a
proprietary Python-only wheel built against the Rockchip NPU runtime. This
module is the Python side that owns it. It listens on
``/run/ados/vision-rknn.sock`` and answers ``load_model`` / ``infer`` /
``embed`` / ``unload`` requests
in the :mod:`ados.services.vision.sidecar_protocol` shape, returning detections
already in the Rust ``Detection`` field layout.

rknn-toolkit-lite2 is imported lazily inside :class:`RknnBackend`. When the
wheel is absent (a dev laptop, a board without the NPU runtime), ``load_model``
returns an ``error`` response and the engine falls back to a Rust-side
ONNX Runtime path; the sidecar process itself stays up and serving.

Run as ``python -m ados.services.vision.rknn_sidecar``.

Detection outputs are decoded by :mod:`ados.services.vision.yolo_decode` with
the layout the ``head`` field of the load request names (``yolov8``, the
default; ``yolov5``; ``yolox``, the per-stride raw heads). A frame that is not
the model's input size is letterboxed into it (aspect kept, centred, padded)
and the boxes come back in the frame's own pixels.
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
from ados.services.vision.sidecar_protocol import SidecarServer
from ados.services.vision.yolo_decode import (
    Letterbox,
    decode_yolo_detections,
    letterbox_rgb24_nhwc,
)

log = get_logger("vision-rknn")

DEFAULT_SOCKET = "/run/ados/vision-rknn.sock"

# Default YOLO-style postprocessing thresholds.
DEFAULT_CONF_THRESHOLD = 0.25
DEFAULT_NMS_IOU = 0.45


@dataclass
class _LoadedModel:
    """A resident RKNN runtime plus the metadata needed to label and scale its
    output back onto the source frame."""

    runtime: Any
    input_w: int
    input_h: int
    fmt: str
    class_labels: list[str]
    head: str = "yolov8"


class RknnBackend:
    """Loads ``.rknn`` models with rknn-toolkit-lite2 and runs inference.

    The toolkit import is deferred to :meth:`load_model` so the sidecar starts
    on any host. A missing wheel surfaces as an error response, not a crash.
    """

    def __init__(
        self,
        conf_threshold: float = DEFAULT_CONF_THRESHOLD,
        nms_iou: float = DEFAULT_NMS_IOU,
    ) -> None:
        self._models: dict[str, _LoadedModel] = {}
        self._conf_threshold = conf_threshold
        self._nms_iou = nms_iou

    @staticmethod
    def _import_runtime() -> Any:
        """Import the RKNN lite runtime class, or raise a clear error."""
        try:
            from rknnlite.api import RKNNLite  # type: ignore[import-not-found]
        except ImportError as exc:  # pragma: no cover - depends on board wheel
            raise RuntimeError(
                "rknn-toolkit-lite2 is not installed; RKNN inference is "
                "unavailable on this host"
            ) from exc
        return RKNNLite

    def load_model(self, req: proto.LoadModelRequest) -> dict[str, Any]:
        path = Path(req.path)
        if not path.is_file():
            return proto.error_response(f"model file not found: {req.path}")

        try:
            rknn_lite_cls = self._import_runtime()
        except RuntimeError as exc:
            return proto.error_response(str(exc))

        try:
            runtime = rknn_lite_cls()
        except Exception as exc:  # pragma: no cover - depends on board wheel
            log.error("rknn_load_failed", model=req.model_id, error=str(exc))
            return proto.error_response(f"rknn load error: {exc}")
        try:
            ret = runtime.load_rknn(str(path))
            if ret != 0:
                proto.release_runtime(runtime, log, model=req.model_id)
                return proto.error_response(f"load_rknn failed (code {ret}) for {req.path}")
            # Auto target picks the NPU core layout for the running SoC.
            ret = runtime.init_runtime()
            if ret != 0:
                proto.release_runtime(runtime, log, model=req.model_id)
                return proto.error_response(f"init_runtime failed (code {ret})")
        except Exception as exc:  # pragma: no cover - depends on board wheel
            proto.release_runtime(runtime, log, model=req.model_id)
            log.error("rknn_load_failed", model=req.model_id, error=str(exc))
            return proto.error_response(f"rknn load error: {exc}")

        # A reload of a resident id replaces it: the new runtime is up, so the
        # old one is released rather than leaked on the NPU.
        previous = self._models.get(req.model_id)
        self._models[req.model_id] = _LoadedModel(
            runtime=runtime,
            input_w=req.input_w,
            input_h=req.input_h,
            fmt=req.format,
            class_labels=req.class_labels,
            head=req.head,
        )
        if previous is not None:
            proto.release_runtime(previous.runtime, log, model=req.model_id)
        log.info("rknn_model_loaded", model=req.model_id, path=req.path)
        return proto.ok_response()

    def unload(self, req: proto.UnloadRequest) -> dict[str, Any]:
        model = self._models.pop(req.model_id, None)
        if model is not None:
            proto.release_runtime(model.runtime, log, model=req.model_id)
            log.info("rknn_model_unloaded", model=req.model_id)
        return proto.ok_response(unloaded=model is not None)

    def infer(self, req: proto.InferRequest) -> dict[str, Any]:
        model = self._models.get(req.model_id)
        if model is None:
            return proto.error_response(f"model not loaded: {req.model_id}")

        try:
            import numpy as np  # lazy: numpy only needed on the real infer path
        except ImportError as exc:  # pragma: no cover - board has numpy
            return proto.error_response(f"numpy unavailable: {exc}")

        try:
            fit = None
            if model.fmt == "rgb24" and (req.width, req.height) != (
                model.input_w,
                model.input_h,
            ) and model.input_w > 0 and model.input_h > 0:
                tensor, fit = letterbox_rgb24_nhwc(
                    np, req.frame, req.width, req.height, model.input_w, model.input_h
                )
            else:
                tensor = self._frame_to_input(np, req, model)
            outputs = model.runtime.inference(inputs=[tensor])
        except Exception as exc:  # pragma: no cover - depends on board wheel
            log.error("rknn_infer_failed", model=req.model_id, error=str(exc))
            return proto.error_response(f"rknn inference error: {exc}")

        detections = self._postprocess_yolo(
            np, outputs, req.width, req.height, model, fit
        )
        return proto.ok_response(detections)

    def dense(self, req: proto.DenseRequest) -> dict[str, Any]:
        """Run a resident depth model over one frame and return its per-pixel
        relative inverse depth. The frame is resized (nearest sample, the whole
        frame squeezed) to the model's input; the converted ``.rknn`` folds the
        ImageNet normalization, so raw uint8 RGB is fed. The output's last two
        axes are the map."""
        model = self._models.get(req.model_id)
        if model is None:
            return proto.error_response(f"model not loaded: {req.model_id}")
        try:
            import numpy as np  # lazy: numpy only needed on the real infer path
        except ImportError as exc:  # pragma: no cover - board has numpy
            return proto.error_response(f"numpy unavailable: {exc}")
        if req.format != "rgb24":
            return proto.error_response(f"depth needs rgb24 frames, got {req.format}")
        try:
            tensor = resize_rgb24_nhwc(
                np, req.frame, req.width, req.height, model.input_w, model.input_h
            )
            outputs = model.runtime.inference(inputs=[tensor])
        except Exception as exc:  # pragma: no cover - depends on board wheel
            log.error("rknn_dense_failed", model=req.model_id, error=str(exc))
            return proto.error_response(f"rknn depth inference error: {exc}")
        arr = np.asarray(outputs[0], dtype="<f4")
        if arr.ndim < 2:
            return proto.error_response(f"depth output has shape {arr.shape}")
        h, w = arr.shape[-2], arr.shape[-1]
        return proto.dense_response(w, h, arr.reshape(h, w).tobytes())

    def embed(self, req: proto.EmbedRequest) -> dict[str, Any]:
        """Run a resident re-id model over one pre-cropped box and return its
        embedding. The crop arrives already cropped + resized to the model's
        input (the engine does the crop so the ONNX and RKNN paths consume
        identical bytes); the converted ``.rknn`` folds the appearance model's
        normalization, so a raw uint8 crop is fed. The output tensor is flattened
        to a 1-D embedding; the engine L2-normalizes it."""
        model = self._models.get(req.model_id)
        if model is None:
            return proto.error_response(f"model not loaded: {req.model_id}")

        try:
            import numpy as np  # lazy: numpy only needed on the real embed path
        except ImportError as exc:  # pragma: no cover - board has numpy
            return proto.error_response(f"numpy unavailable: {exc}")

        try:
            # The crop reshapes to NHWC uint8 exactly like a detector frame.
            crop_frame = proto.InferRequest(
                model_id=req.model_id,
                frame=req.crop,
                width=req.crop_w,
                height=req.crop_h,
                format=req.format,
            )
            tensor = self._frame_to_input(np, crop_frame, model)
            outputs = model.runtime.inference(inputs=[tensor])
        except Exception as exc:  # pragma: no cover - depends on board wheel
            log.error("rknn_embed_failed", model=req.model_id, error=str(exc))
            return proto.error_response(f"rknn embed error: {exc}")

        if not outputs:
            return proto.error_response("rknn embed produced no output")
        embedding = np.asarray(outputs[0], dtype=np.float32).reshape(-1).tolist()
        return proto.embedding_response(embedding)

    @staticmethod
    def _frame_to_input(np: Any, req: proto.InferRequest, model: _LoadedModel) -> Any:
        """Reshape a raw RGB24 frame into an NHWC uint8 tensor.

        Only ``rgb24`` is reshaped here; NV12/YUV420p frames are passed through
        as-is for runtimes that accept the planar buffer, which keeps this path
        free of a colour-conversion dependency.
        """
        buf = np.frombuffer(req.frame, dtype=np.uint8)
        if model.fmt == "rgb24":
            expected = req.width * req.height * 3
            if buf.size < expected:
                raise ValueError(
                    f"rgb24 frame is {buf.size} bytes, expected {expected}"
                )
            return buf[:expected].reshape((1, req.height, req.width, 3))
        return buf.reshape((1, -1))

    def _postprocess_yolo(
        self,
        np: Any,
        outputs: list[Any],
        frame_w: int,
        frame_h: int,
        model: _LoadedModel,
        fit: Letterbox | None = None,
    ) -> list[dict[str, Any]]:
        """Decode this model's head to detections in source-frame pixels
        (:func:`decode_yolo_detections` with ``model.head``); ``fit`` is the
        letterbox the frame went in with, if it was letterboxed."""
        return decode_yolo_detections(
            np,
            outputs,
            frame_w=frame_w,
            frame_h=frame_h,
            input_w=model.input_w,
            input_h=model.input_h,
            class_labels=model.class_labels,
            head=model.head,
            conf_threshold=self._conf_threshold,
            nms_iou=self._nms_iou,
            fit=fit,
        )


async def serve(socket_path: str = DEFAULT_SOCKET) -> None:
    """Bind the RKNN sidecar socket and serve until cancelled."""
    backend = RknnBackend()
    server = SidecarServer(socket_path, backend, log)
    await server.serve_forever()


def _run() -> None:
    parser = argparse.ArgumentParser(description="ADOS RKNN inference sidecar")
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


def resize_rgb24_nhwc(
    np: Any, frame: bytes, width: int, height: int, out_w: int, out_h: int
) -> Any:
    """Nearest-sample an RGB24 frame to ``out_w`` x ``out_h`` as a
    ``(1, out_h, out_w, 3)`` uint8 tensor, the whole frame squeezed in."""
    buf = np.frombuffer(frame, dtype=np.uint8)
    expected = width * height * 3
    if buf.size < expected:
        raise ValueError(f"rgb24 frame is {buf.size} bytes, expected {expected}")
    img = buf[:expected].reshape((height, width, 3))
    ys = (np.arange(out_h) * height) // out_h
    xs = (np.arange(out_w) * width) // out_w
    return np.ascontiguousarray(img[ys][:, xs]).reshape((1, out_h, out_w, 3))


if __name__ == "__main__":
    _run()
