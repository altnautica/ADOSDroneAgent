#!/usr/bin/env python3
"""Export the Depth Anything V2 Small depth model the vision engine runs.

Inputs: the upstream Depth Anything V2 source tree (Apache-2.0) at commit
``a561b849ebae10a6f5ef49e26c83cbbcd36c71bf`` and its Small checkpoint
``depth_anything_v2_vits.pth`` (Apache-2.0; checked against its sha256).
Output: one ONNX graph the engine's depth layer reads:

* input ``image``: float32 ``[1, 3, H, W]``, RGB scaled to ``[0, 1]`` then
  normalised with the ImageNet mean and standard deviation, the engine's
  depth preprocessing. H and W must be multiples of 14 (the ViT patch); the
  default 252x196 is the one nearest 256x192.
* output ``depth``: float32 ``[1, H, W]``, relative inverse depth (larger is
  nearer, scale unknown); the engine scales it to metres with ranging hits.

The export runs on the CPU (it needs torch, torchvision, onnx, onnxruntime and
opencv-python-headless), checks the ONNX output against PyTorch on three fixed
inputs (noise, a sky-over-ground scene, a ramp), and prints the file size and
sha256 that the model registry records.

Usage::

    python export_depth_anything_v2.py --src <Depth-Anything-V2 tree> \\
        --checkpoint depth_anything_v2_vits.pth \\
        --out depth_anything_v2_small_252x196.onnx
"""

from __future__ import annotations

import argparse
import hashlib
import sys
from pathlib import Path

CHECKPOINT_SHA256 = "715fade13be8f229f8a70cc02066f656f2423a59effd0579197bbf57860e1378"
# The Small (ViT-S) configuration (upstream run.py, model_configs["vits"]).
VITS = {"encoder": "vits", "features": 64, "out_channels": [48, 96, 192, 384]}
PATCH = 14
IMAGENET_MEAN = (0.485, 0.456, 0.406)
IMAGENET_STD = (0.229, 0.224, 0.225)


def _sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def check_inputs(width: int, height: int):  # noqa: ANN201 - list of (name, ndarray)
    """Normalised check inputs: noise, a sky-over-ground scene, a ramp."""
    import numpy as np

    rng = np.random.default_rng(7)
    mean = np.array(IMAGENET_MEAN, dtype=np.float32).reshape(3, 1, 1)
    std = np.array(IMAGENET_STD, dtype=np.float32).reshape(3, 1, 1)

    def norm(rgb01):  # noqa: ANN001, ANN202
        return ((rgb01 - mean) / std)[None].astype(np.float32)

    noise = rng.random((3, height, width), dtype=np.float32)
    scene = np.zeros((3, height, width), dtype=np.float32)
    scene[:, : height // 2] = np.array([0.6, 0.75, 0.92], dtype=np.float32)[:, None, None]
    ys, xs = np.mgrid[height // 2 : height, 0:width]
    grain = 2 + (ys - height // 2) // 6
    ground = 0.35 + 0.2 * (((xs // grain) + (ys // grain)) % 2).astype(np.float32)
    scene[0, height // 2 :] = ground
    scene[1, height // 2 :] = ground * 0.8
    scene[2, height // 2 :] = ground * 0.5
    ramp = np.broadcast_to(
        np.linspace(0, 1, height, dtype=np.float32)[None, :, None], (3, height, width)
    ).copy()
    return [("noise", norm(noise)), ("scene", norm(scene)), ("ramp", norm(ramp))]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--src", type=Path, required=True, help="Depth Anything V2 source tree")
    ap.add_argument("--checkpoint", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--width", type=int, default=252)
    ap.add_argument("--height", type=int, default=196)
    ap.add_argument("--opset", type=int, default=17)
    ap.add_argument(
        "--tolerance",
        type=float,
        default=1e-3,
        help="largest |onnx - torch| allowed, as a share of the torch output range",
    )
    args = ap.parse_args()
    if args.width % PATCH or args.height % PATCH:
        print(f"input {args.width}x{args.height} is not a multiple of {PATCH}", file=sys.stderr)
        return 1
    got_sha = _sha256(args.checkpoint)
    if got_sha != CHECKPOINT_SHA256:
        print(f"checkpoint sha256 {got_sha} is not {CHECKPOINT_SHA256}", file=sys.stderr)
        return 1

    import numpy as np
    import onnx
    import onnxruntime as ort
    import torch

    sys.path.insert(0, str(args.src))
    from depth_anything_v2.dpt import DepthAnythingV2

    torch.set_grad_enabled(False)
    model = DepthAnythingV2(**VITS)
    model.load_state_dict(torch.load(args.checkpoint, map_location="cpu", weights_only=True))
    model.eval()

    args.out.parent.mkdir(parents=True, exist_ok=True)
    torch.onnx.export(
        model,
        (torch.zeros(1, 3, args.height, args.width),),
        str(args.out),
        input_names=["image"],
        output_names=["depth"],
        opset_version=args.opset,
        do_constant_folding=True,
        dynamo=False,
    )
    onnx.checker.check_model(str(args.out))

    session = ort.InferenceSession(str(args.out), providers=["CPUExecutionProvider"])
    worst = 0.0
    for name, x in check_inputs(args.width, args.height):
        want = model(torch.from_numpy(x)).numpy()
        got = session.run(None, {"image": x})[0]
        if got.shape != want.shape or got.shape != (1, args.height, args.width):
            print(f"{name}: shape mismatch: onnx {got.shape} torch {want.shape}", file=sys.stderr)
            return 1
        span = float(want.max() - want.min()) or 1.0
        diff = float(np.abs(got - want).max())
        print(f"{name}: max abs diff vs torch {diff:.3e} over a range of {span:.3g}")
        worst = max(worst, diff / span)
    if worst > args.tolerance:
        print("onnx output diverges from torch", file=sys.stderr)
        return 1
    print(f"size_bytes {args.out.stat().st_size}")
    print(f"sha256 {_sha256(args.out)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
