#!/usr/bin/env python3
"""Export the YOLOX-Tiny person detector the CPU (ONNX) vision backend ships.

Inputs: an upstream YOLOX source checkout (Apache-2.0) and its released
``yolox_tiny.pth`` checkpoint (Apache-2.0). Output: one ONNX graph that the
vision engine's ``yolo5`` head decoder reads without any extra code:

* input ``images``: float32 ``[1, 3, S, S]``, RGB, scaled to ``[0, 1]``. This is
  the engine's standard detector preprocessing; the graph itself converts it to
  the BGR ``0..255`` input the checkpoint was trained on.
* output ``output``: float32 ``[1, anchors, 6]``. Per anchor: ``cx, cy, w, h``
  in input pixels, objectness, person score (both already sigmoid). The other
  79 COCO class columns are dropped, so the model is person-only.

The export runs on the CPU, stamps the graph with the ``ados.*`` metadata the
engine's ONNX backend reads at load (head layout, class list, input size), so
no config writer has to know them, checks the ONNX output against PyTorch on a
fixed random input, and prints the file size and sha256 that the model
registry records.

Usage::

    python export_yolox_person.py --yolox-src <YOLOX checkout> \\
        --checkpoint yolox_tiny.pth --out yolox_tiny_person_640.onnx
"""

from __future__ import annotations

import argparse
import hashlib
import sys
from pathlib import Path

# The COCO index of "person" in the YOLOX class order.
PERSON_CLASS = 0
# YOLOX-Tiny scaling (exps/default/yolox_tiny.py).
TINY_DEPTH = 0.33
TINY_WIDTH = 0.375


def _sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def build(yolox_src: Path, checkpoint: Path):  # noqa: ANN201 - torch types are optional deps
    import torch
    from torch import nn

    sys.path.insert(0, str(yolox_src))
    from yolox.models import YOLOPAFPN, YOLOX, YOLOXHead

    in_channels = [256, 512, 1024]
    backbone = YOLOPAFPN(TINY_DEPTH, TINY_WIDTH, in_channels=in_channels, act="silu")
    head = YOLOXHead(80, TINY_WIDTH, in_channels=in_channels, act="silu")
    model = YOLOX(backbone, head)
    ckpt = torch.load(str(checkpoint), map_location="cpu", weights_only=False)
    state = ckpt["model"] if isinstance(ckpt, dict) and "model" in ckpt else ckpt
    model.load_state_dict(state)
    model.eval()
    # Decode inside the graph: boxes come out in input pixels, scores as
    # probabilities, matching the YOLOv5-style head the engine decodes.
    model.head.decode_in_inference = True

    class PersonOnly(nn.Module):
        def __init__(self, inner: nn.Module) -> None:
            super().__init__()
            self.inner = inner

        def forward(self, x: torch.Tensor) -> torch.Tensor:
            # RGB [0, 1] -> BGR [0, 255], the checkpoint's training input.
            bgr = torch.flip(x, dims=[1]) * 255.0
            out = self.inner(bgr)
            keep = 5 + PERSON_CLASS
            return torch.cat([out[..., :5], out[..., keep : keep + 1]], dim=-1)

    return PersonOnly(model).eval()


def stamp_metadata(graph, props: dict[str, str]) -> None:  # noqa: ANN001 - onnx.ModelProto
    """Replace the graph's ``ados.*`` metadata entries with ``props``."""
    kept = [p for p in graph.metadata_props if not p.key.startswith("ados.")]
    del graph.metadata_props[:]
    graph.metadata_props.extend(kept)
    for key, value in sorted(props.items()):
        entry = graph.metadata_props.add()
        entry.key = key
        entry.value = value


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--yolox-src", type=Path, required=True)
    ap.add_argument("--checkpoint", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--size", type=int, default=640)
    ap.add_argument("--opset", type=int, default=13)
    args = ap.parse_args()

    import numpy as np
    import onnx
    import onnxruntime as ort
    import torch

    model = build(args.yolox_src, args.checkpoint)
    dummy = torch.zeros(1, 3, args.size, args.size)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    torch.onnx.export(
        model,
        dummy,
        str(args.out),
        input_names=["images"],
        output_names=["output"],
        opset_version=args.opset,
        do_constant_folding=True,
        dynamo=False,
    )
    graph = onnx.load(str(args.out))
    stamp_metadata(
        graph,
        {
            "ados.kind": "detection",
            "ados.head": "yolo5",
            "ados.classes": "person",
            "ados.input": f"{args.size}x{args.size}",
        },
    )
    onnx.save(graph, str(args.out))

    rng = np.random.default_rng(7)
    probe = rng.random((1, 3, args.size, args.size), dtype=np.float32)
    with torch.no_grad():
        want = model(torch.from_numpy(probe)).numpy()
    sess = ort.InferenceSession(str(args.out), providers=["CPUExecutionProvider"])
    got = sess.run(None, {"images": probe})[0]
    if got.shape != want.shape:
        print(f"shape mismatch: onnx {got.shape} torch {want.shape}", file=sys.stderr)
        return 1
    err = float(np.max(np.abs(got - want)))
    print(f"output shape {tuple(got.shape)}, max abs diff vs torch {err:.3e}")
    if err > 1e-2:
        print("onnx output diverges from torch", file=sys.stderr)
        return 1
    print(f"size_bytes {args.out.stat().st_size}")
    print(f"sha256 {_sha256(args.out)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
