#!/usr/bin/env python3
"""Export a YOLOX person detector (Tiny or S) the vision backends ship.

Inputs: an upstream YOLOX source checkout (Apache-2.0) and its released
``yolox_tiny.pth`` or ``yolox_s.pth`` checkpoint (Apache-2.0). Output: one ONNX
graph that the vision engine's ``yolo5`` head decoder reads without any extra
code:

* input ``images``: float32 ``[1, 3, S, S]``, RGB, scaled to ``[0, 1]``. This is
  the engine's standard detector preprocessing; the graph itself converts it to
  the BGR ``0..255`` input the checkpoint was trained on.
* output ``output``: float32 ``[1, anchors, 6]``. Per anchor: ``cx, cy, w, h``
  in input pixels, objectness, person score (both already sigmoid). The other
  79 COCO class columns are dropped, so the model is person-only.

With ``--raw-heads`` the graph stops after the convolutions (head ``yolox``),
the form the RKNN toolkit compiles: outputs ``p8``, ``p16``, ``p32``, float32
``[1, 6, S/8, S/8]`` and so on, each cell ``dx, dy`` from the cell and ``log w,
log h`` in stride units, then sigmoid objectness and person score. The NPU
sidecar decodes them; the engine's ONNX backend does not read this form.

The export runs on the CPU, stamps the graph with the ``ados.*`` metadata the
engine's ONNX backend reads at load (head layout, class list, input size), so
no config writer has to know them, checks the ONNX output against PyTorch on a
fixed random input, and prints the file size and sha256 that the model
registry records.

Usage::

    python export_yolox_person.py --yolox-src <YOLOX checkout> --model tiny \\
        --checkpoint yolox_tiny.pth --out yolox_tiny_person_640.onnx
    python export_yolox_person.py --yolox-src <YOLOX checkout> --model s \\
        --checkpoint yolox_s.pth --out yolox_s_person_640.onnx
    python export_yolox_person.py --yolox-src <YOLOX checkout> --model s \\
        --checkpoint yolox_s.pth --raw-heads --out yolox_s_person_heads_640.onnx
"""

from __future__ import annotations

import argparse
import hashlib
import inspect
import sys
from pathlib import Path

# The COCO index of "person" in the YOLOX class order.
PERSON_CLASS = 0
# (depth, width) multipliers per model (exps/default/yolox_tiny.py, yolox_s.py).
SCALING = {"tiny": (0.33, 0.375), "s": (0.33, 0.50)}


def _sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def build(yolox_src: Path, checkpoint: Path, variant: str, raw_heads: bool = False):  # noqa: ANN201
    import torch
    from torch import nn

    sys.path.insert(0, str(yolox_src))
    # yolox.utils imports training-only packages (torchvision, cv2); the
    # inference graph needs only its meshgrid, so the package is stubbed.
    import types

    utils = types.ModuleType("yolox.utils")
    utils.meshgrid = lambda *t: torch.meshgrid(*t, indexing="ij")
    utils.bboxes_iou = utils.cxcywh2xyxy = utils.visualize_assign = None
    sys.modules["yolox.utils"] = utils
    from yolox.models import YOLOPAFPN, YOLOX, YOLOXHead

    depth, width = SCALING[variant]
    in_channels = [256, 512, 1024]
    backbone = YOLOPAFPN(depth, width, in_channels=in_channels, act="silu")
    head = YOLOXHead(80, width, in_channels=in_channels, act="silu")
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

    class PersonRawHeads(nn.Module):
        """One output per pyramid level (strides 8, 16, 32): ``[1, 6, H, W]``
        with the box regression as trained (``dx, dy`` from the cell, ``log w,
        log h`` in stride units), then sigmoid objectness and person score. No
        reshape, concat or transpose follows the convolutions, which is the
        form the RKNN toolkit compiles; the consumer decodes the boxes
        (``yolox`` head)."""

        def __init__(self, inner: nn.Module) -> None:
            super().__init__()
            self.inner = inner

        def forward(self, x: torch.Tensor):  # noqa: ANN201 - a tuple of tensors
            bgr = torch.flip(x, dims=[1]) * 255.0
            head = self.inner.head
            outs = []
            for k, feat in enumerate(self.inner.backbone(bgr)):
                feat = head.stems[k](feat)
                cls = head.cls_preds[k](head.cls_convs[k](feat))
                reg_feat = head.reg_convs[k](feat)
                reg = head.reg_preds[k](reg_feat)
                obj = head.obj_preds[k](reg_feat)
                person = cls[:, PERSON_CLASS : PERSON_CLASS + 1]
                outs.append(torch.cat([reg, obj.sigmoid(), person.sigmoid()], dim=1))
            return tuple(outs)

    return (PersonRawHeads(model) if raw_heads else PersonOnly(model)).eval()


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
    ap.add_argument("--model", choices=sorted(SCALING), default="tiny")
    ap.add_argument("--checkpoint", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--size", type=int, default=640)
    ap.add_argument("--opset", type=int, default=13)
    ap.add_argument(
        "--raw-heads",
        action="store_true",
        help="one undecoded [1, 6, H, W] output per stride (the NPU form, head 'yolox')",
    )
    args = ap.parse_args()

    import numpy as np
    import onnx
    import onnxruntime as ort
    import torch

    model = build(args.yolox_src, args.checkpoint, args.model, args.raw_heads)
    dummy = torch.zeros(1, 3, args.size, args.size)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    outputs = ["p8", "p16", "p32"] if args.raw_heads else ["output"]
    # The legacy (TorchScript) exporter; `dynamo` only exists from torch 2.5.
    legacy = {"dynamo": False} if "dynamo" in inspect.signature(torch.onnx.export).parameters else {}
    torch.onnx.export(
        model,
        dummy,
        str(args.out),
        input_names=["images"],
        output_names=outputs,
        opset_version=args.opset,
        do_constant_folding=True,
        **legacy,
    )
    graph = onnx.load(str(args.out))
    stamp_metadata(
        graph,
        {
            "ados.kind": "detection",
            "ados.head": "yolox" if args.raw_heads else "yolo5",
            "ados.classes": "person",
            "ados.input": f"{args.size}x{args.size}",
        },
    )
    onnx.save(graph, str(args.out))

    rng = np.random.default_rng(7)
    probe = rng.random((1, 3, args.size, args.size), dtype=np.float32)
    with torch.no_grad():
        res = model(torch.from_numpy(probe))
    wants = [t.numpy() for t in (res if isinstance(res, tuple) else (res,))]
    sess = ort.InferenceSession(str(args.out), providers=["CPUExecutionProvider"])
    gots = sess.run(None, {"images": probe})
    err = 0.0
    for got, want in zip(gots, wants, strict=True):
        if got.shape != want.shape:
            print(f"shape mismatch: onnx {got.shape} torch {want.shape}", file=sys.stderr)
            return 1
        err = max(err, float(np.max(np.abs(got - want))))
    print(f"output shapes {[tuple(g.shape) for g in gots]}, max abs diff vs torch {err:.3e}")
    if err > 1e-2:
        print("onnx output diverges from torch", file=sys.stderr)
        return 1
    print(f"size_bytes {args.out.stat().st_size}")
    print(f"sha256 {_sha256(args.out)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
