#!/usr/bin/env python3
"""Export the OSNet-x0.25 person re-identification embedder to ONNX.

Inputs: an upstream torchreid source checkout (MIT) and an ``osnet_x0_25``
re-id checkpoint from its model zoo. Output: one ONNX graph that the vision
engine's ONNX ``embed`` path runs as-is:

* input ``crop``: float32 ``[1, 3, 256, 128]`` (portrait), RGB, ImageNet
  normalized ``(pixel - mean) / std``. The engine applies this normalization in
  code (``ados_vision::reid::preprocess_osnet_nchw``), not in the graph.
* output ``embedding``: float32 ``[1, 512]``, the raw feature. The engine
  L2-normalizes it before cosine matching.

The export stamps the graph with the ``ados.*`` metadata the engine's ONNX
backend reads at load (model kind, input size), checks the ONNX output against
PyTorch on a fixed random input, and prints the file size and sha256 that the
model registry records.

Usage::

    python export_osnet_reid.py --torchreid-src <deep-person-reid checkout> \\
        --checkpoint osnet_x0_25_msmt17.pth --out osnet_x0_25_reid_256x128.onnx
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import sys
from pathlib import Path

INPUT_H = 256
INPUT_W = 128


def _sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def _load_osnet_module(torchreid_src: Path):  # noqa: ANN202 - optional deps
    # osnet.py depends on torch only; load it by path so the export does not
    # need the full torchreid package (and its compiled evaluation extension).
    path = torchreid_src / "torchreid" / "reid" / "models" / "osnet.py"
    if not path.is_file():
        path = torchreid_src / "torchreid" / "models" / "osnet.py"
    spec = importlib.util.spec_from_file_location("osnet_export_src", path)
    if spec is None or spec.loader is None:
        raise SystemExit(f"cannot load {path}")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def build(torchreid_src: Path, checkpoint: Path):  # noqa: ANN201
    import torch

    osnet = _load_osnet_module(torchreid_src)
    model = osnet.osnet_x0_25(num_classes=1, pretrained=False, loss="softmax")
    ckpt = torch.load(str(checkpoint), map_location="cpu", weights_only=False)
    state = ckpt.get("state_dict", ckpt) if isinstance(ckpt, dict) else ckpt
    cleaned = {}
    for k, v in state.items():
        k = k.removeprefix("module.")
        # The identity classifier is training-only; the embedding is the
        # feature before it.
        if k.startswith("classifier."):
            continue
        cleaned[k] = v
    missing, unexpected = model.load_state_dict(cleaned, strict=False)
    missing = [k for k in missing if not k.startswith("classifier.")]
    if missing or unexpected:
        raise SystemExit(f"checkpoint mismatch: missing={missing} unexpected={unexpected}")
    return model.eval()


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--torchreid-src", type=Path, required=True)
    ap.add_argument("--checkpoint", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--opset", type=int, default=13)
    args = ap.parse_args()

    import numpy as np
    import onnx
    import onnxruntime as ort
    import torch

    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from export_yolox_person import stamp_metadata

    model = build(args.torchreid_src, args.checkpoint)
    dummy = torch.zeros(1, 3, INPUT_H, INPUT_W)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    torch.onnx.export(
        model,
        dummy,
        str(args.out),
        input_names=["crop"],
        output_names=["embedding"],
        opset_version=args.opset,
        do_constant_folding=True,
        dynamo=False,
    )
    graph = onnx.load(str(args.out))
    stamp_metadata(graph, {"ados.kind": "reid", "ados.input": f"{INPUT_W}x{INPUT_H}"})
    onnx.save(graph, str(args.out))

    rng = np.random.default_rng(7)
    probe = rng.standard_normal((1, 3, INPUT_H, INPUT_W), dtype=np.float32)
    with torch.no_grad():
        want = model(torch.from_numpy(probe)).numpy()
    sess = ort.InferenceSession(str(args.out), providers=["CPUExecutionProvider"])
    got = sess.run(None, {"crop": probe})[0]
    if got.shape != want.shape:
        print(f"shape mismatch: onnx {got.shape} torch {want.shape}", file=sys.stderr)
        return 1
    err = float(np.max(np.abs(got - want)))
    print(f"output shape {tuple(got.shape)}, max abs diff vs torch {err:.3e}")
    if err > 1e-3:
        print("onnx output diverges from torch", file=sys.stderr)
        return 1
    print(f"size_bytes {args.out.stat().st_size}")
    print(f"sha256 {_sha256(args.out)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
