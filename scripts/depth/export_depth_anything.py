#!/usr/bin/env python3
"""Export Depth Anything V2 Small to ONNX for the vision engine's depth layer.

The engine runs a monocular relative-depth model and scales it to metres with
LiDAR and rangefinder hits. This script builds that model file:

* code: the official Depth Anything V2 repository at a pinned commit
  (Apache-2.0);
* weights: the Small checkpoint `depth_anything_v2_vits.pth` (Apache-2.0).
  The Base and Large checkpoints and the metric fine-tunes carry
  non-commercial or share-alike licences and are never used here.

The exported graph takes `image` float32 `[1, 3, H, W]` (RGB, `/255`, then the
ImageNet mean and std, exactly what the engine's depth preprocessing writes)
and returns `depth` float32 `[1, H, W]`: relative inverse depth, larger is
nearer. H and W must be multiples of 14 (the ViT patch); the default
252x196 is the 14-multiple nearest 256x192.

The script checks the exported graph against PyTorch on several inputs and
refuses to write a model whose outputs differ by more than `--tolerance`
(relative to the output range), then writes `<stem>.onnx` and `<stem>.json`
(size, sha256, licence, input size, the check's numbers) into `--out`.

It needs torch (CPU is enough), onnx, onnxruntime and opencv-python-headless.
A throwaway container works:

    docker run --rm --network host -v "$PWD/scripts/depth:/s:ro" \\
      -v "$HOME/.cache/cameradepth:/w" python:3.12-slim-trixie sh -c '
        pip install -q --index-url https://download.pytorch.org/whl/cpu torch &&
        pip install -q onnx onnxruntime onnxscript opencv-python-headless &&
        python /s/export_depth_anything.py --work /w/src --out /w/out'
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import sys
import tarfile
import time
import urllib.request
from pathlib import Path

REPO_COMMIT = "a561b849ebae10a6f5ef49e26c83cbbcd36c71bf"
REPO_URL = (
    "https://github.com/DepthAnything/Depth-Anything-V2/archive/"
    f"{REPO_COMMIT}.tar.gz"
)
CKPT_URL = (
    "https://huggingface.co/depth-anything/Depth-Anything-V2-Small/"
    "resolve/main/depth_anything_v2_vits.pth"
)
CKPT_SHA256 = "715fade13be8f229f8a70cc02066f656f2423a59effd0579197bbf57860e1378"
VITS_CONFIG = {"encoder": "vits", "features": 64, "out_channels": [48, 96, 192, 384]}
PATCH = 14
IMAGENET_MEAN = (0.485, 0.456, 0.406)
IMAGENET_STD = (0.229, 0.224, 0.225)


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def fetch_repo(work: Path) -> Path:
    """The pinned repository tree under `work`, downloaded once."""
    root = work / f"Depth-Anything-V2-{REPO_COMMIT}"
    if not (root / "depth_anything_v2" / "dpt.py").is_file():
        print(f"fetching {REPO_URL}", flush=True)
        with urllib.request.urlopen(REPO_URL) as r:
            data = r.read()
        with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as tar:
            tar.extractall(work, filter="data")
    return root


def fetch_checkpoint(work: Path) -> Path:
    """The Small checkpoint under `work`, downloaded once and hash-checked."""
    path = work / "vits.pth"
    if not path.is_file():
        print(f"fetching {CKPT_URL}", flush=True)
        urllib.request.urlretrieve(CKPT_URL, path)
    got = sha256(path)
    if got != CKPT_SHA256:
        sys.exit(f"checkpoint sha256 {got} is not the pinned {CKPT_SHA256}")
    return path


def check_inputs(width: int, height: int):
    """Inputs the check runs on: noise, a sky-over-ground scene, a ramp."""
    import numpy as np

    rng = np.random.default_rng(7)
    mean = np.array(IMAGENET_MEAN, dtype=np.float32).reshape(3, 1, 1)
    std = np.array(IMAGENET_STD, dtype=np.float32).reshape(3, 1, 1)

    def norm(rgb01):
        return ((rgb01 - mean) / std)[None].astype(np.float32)

    noise = rng.random((3, height, width), dtype=np.float32)
    scene = np.zeros((3, height, width), dtype=np.float32)
    scene[:, : height // 2] = np.array([0.6, 0.75, 0.92], dtype=np.float32)[:, None, None]
    ys, xs = np.mgrid[height // 2 : height, 0:width]
    grain = 2 + (ys - height // 2) // 6
    checker = (((xs // grain) + (ys // grain)) % 2).astype(np.float32)
    ground = 0.35 + 0.2 * checker
    scene[0, height // 2 :] = ground
    scene[1, height // 2 :] = ground * 0.8
    scene[2, height // 2 :] = ground * 0.5
    ramp = np.broadcast_to(
        np.linspace(0, 1, height, dtype=np.float32)[None, :, None], (3, height, width)
    ).copy()
    return [("noise", norm(noise)), ("scene", norm(scene)), ("ramp", norm(ramp))]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--work", type=Path, required=True, help="download cache")
    ap.add_argument("--out", type=Path, required=True, help="output directory")
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
        sys.exit(f"input {args.width}x{args.height} is not a multiple of {PATCH}")
    args.work.mkdir(parents=True, exist_ok=True)
    args.out.mkdir(parents=True, exist_ok=True)

    repo = fetch_repo(args.work)
    ckpt = fetch_checkpoint(args.work)
    sys.path.insert(0, str(repo))

    import numpy as np
    import onnxruntime as ort
    import torch
    from depth_anything_v2.dpt import DepthAnythingV2

    torch.set_grad_enabled(False)
    model = DepthAnythingV2(**VITS_CONFIG)
    model.load_state_dict(torch.load(ckpt, map_location="cpu", weights_only=True))
    model.eval()

    stem = f"depth_anything_v2_small_{args.width}x{args.height}"
    onnx_path = args.out / f"{stem}.onnx"
    dummy = torch.zeros(1, 3, args.height, args.width)
    torch.onnx.export(
        model,
        (dummy,),
        str(onnx_path),
        input_names=["image"],
        output_names=["depth"],
        opset_version=args.opset,
        do_constant_folding=True,
        dynamo=False,
    )

    import onnx

    onnx.checker.check_model(str(onnx_path))
    session = ort.InferenceSession(str(onnx_path), providers=["CPUExecutionProvider"])
    out_shape = session.get_outputs()[0].shape
    checks = []
    for name, x in check_inputs(args.width, args.height):
        want = model(torch.from_numpy(x)).numpy()
        got = session.run(None, {"image": x})[0]
        if got.shape != (1, args.height, args.width) or got.shape != want.shape:
            sys.exit(f"{name}: onnx output {got.shape}, torch {want.shape}")
        span = float(want.max() - want.min()) or 1.0
        diff = float(np.abs(got - want).max())
        checks.append({"input": name, "max_abs_diff": diff, "torch_range": span})
        print(f"{name}: max |onnx - torch| {diff:.3g} over a range of {span:.3g}", flush=True)
        if diff > args.tolerance * span:
            onnx_path.unlink()
            sys.exit(f"{name}: onnx differs from torch by {diff} (> {args.tolerance} x {span})")

    x = check_inputs(args.width, args.height)[1][1]
    for _ in range(3):
        session.run(None, {"image": x})
    times = []
    for _ in range(20):
        t0 = time.perf_counter()
        session.run(None, {"image": x})
        times.append((time.perf_counter() - t0) * 1000.0)
    times.sort()

    record = {
        "id": "depth_anything_v2_small",
        "filename": onnx_path.name,
        "input_size": f"{args.width}x{args.height}",
        "input": {"name": "image", "shape": [1, 3, args.height, args.width]},
        "output": {"name": "depth", "shape": out_shape, "meaning": "relative inverse depth"},
        "size_bytes": onnx_path.stat().st_size,
        "sha256": sha256(onnx_path),
        "license": "Apache-2.0",
        "source": {"code": REPO_URL, "weights": CKPT_URL, "weights_sha256": CKPT_SHA256},
        "opset": args.opset,
        "torch": torch.__version__,
        "onnxruntime": ort.__version__,
        "check": checks,
        "ort_cpu_ms": {"median": times[len(times) // 2], "max": times[-1], "runs": len(times)},
    }
    (args.out / f"{stem}.json").write_text(json.dumps(record, indent=2) + "\n")
    print(json.dumps(record, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
