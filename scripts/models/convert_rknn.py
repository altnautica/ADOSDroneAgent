#!/usr/bin/env python3
"""Convert a vision ONNX model to RKNN files for the Rockchip NPU sidecar.

Runs on an x86_64 host with ``rknn-toolkit2`` 2.3.2 installed (the toolkit
version the board-side runtime and ``rknn-toolkit-lite2`` wheel are pinned to;
the runtime rejects a model exported by another version). One ``.rknn`` file is
written per target platform: a model compiled for one NPU generation does not
load on another, so the registry lists one variant per platform.

Precision is fp16, not int8. An int8 build needs a calibration image set, and
each person-detector output carries box regressions beside probabilities
(0..1) in one tensor: per-tensor int8 output quantisation would flatten the
scores. The RK3588 and RK3576 NPUs run fp16 natively. The detector compiled
here is the raw-heads export (``export_yolox_person.py --raw-heads``, one
output per stride): the toolkit cannot compile the in-graph box decoder, and
the sidecar decodes the heads (head ``yolox``).

The NPU sidecar feeds raw ``uint8`` RGB frames in NHWC order, so the RKNN graph
takes over the ONNX graph's input normalisation: ``--mean`` and ``--std`` are
the per-channel values the toolkit applies (``x = (pixel - mean) / std``). The
defaults (0, 255) match the detectors' ``[0, 1]`` RGB input.

Each converted file is checked against ONNX Runtime on three synthetic images
through the toolkit's simulator (every output, relative to its range), and its
size and sha256 are printed for the model registry.

Usage::

    python convert_rknn.py --onnx yolox_s_person_heads_640.onnx \\
        --platform rk3588 --platform rk3576 --out-dir out/ --tolerance 0.05
"""

from __future__ import annotations

import argparse
import hashlib
import sys
from pathlib import Path

# The toolkit version every board-side runtime is pinned to.
TOOLKIT_VERSION = "2.3.2"
PLATFORMS = ("rk3588", "rk3576")


def _sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def _toolkit_version() -> str:
    from importlib.metadata import version

    return version("rknn-toolkit2")


def _onnx_input(onnx_path: Path) -> tuple[str, int, int]:
    """The graph's single input name and its height and width (NCHW)."""
    import onnx

    graph = onnx.load(str(onnx_path)).graph
    if len(graph.input) != 1:
        raise SystemExit(f"{onnx_path}: expected one input, found {len(graph.input)}")
    inp = graph.input[0]
    dims = [d.dim_value for d in inp.type.tensor_type.shape.dim]
    if len(dims) != 4 or dims[1] != 3 or dims[2] <= 0 or dims[3] <= 0:
        raise SystemExit(f"{onnx_path}: expected a static [1, 3, H, W] input, got {dims}")
    return inp.name, dims[2], dims[3]


def _cascade_maxpools(graph) -> None:  # noqa: ANN001 - onnx.GraphProto
    """Replace each stride-1, same-padded square ``MaxPool`` of kernel 9, 13,
    ... (``4n + 1``) by ``n`` chained 5x5 pools: the same values (a max over a
    window is the max of maxes over windows that cover it, as in SPPF), but no
    kernel the toolkit tiles; its tiling of the YOLOX spatial-pyramid pools
    breaks the build."""
    from onnx import helper

    nodes = list(graph.node)
    del graph.node[:]
    for node in nodes:
        attrs = {a.name: helper.get_attribute_value(a) for a in node.attribute}
        kernel = list(attrs.pop("kernel_shape", []))
        k = kernel[0] if len(kernel) == 2 and kernel[0] == kernel[1] else 0
        # The exporter also writes the defaults (ceil_mode 0, unit dilations).
        defaults = {"strides": [1, 1], "dilations": [1, 1], "ceil_mode": 0, "storage_order": 0}
        if (
            node.op_type != "MaxPool"
            or k <= 5
            or (k - 1) % 4
            or list(attrs.pop("pads", [])) != [k // 2] * 4
            or len(node.output) != 1
            or any(
                name not in defaults or (list(v) if isinstance(v, list) else v) != defaults[name]
                for name, v in attrs.items()
            )
        ):
            graph.node.append(node)
            continue
        steps = (k - 1) // 4
        src = node.input[0]
        for i in range(steps):
            dst = node.output[0] if i == steps - 1 else f"{node.output[0]}_k5_{i}"
            graph.node.append(
                helper.make_node(
                    "MaxPool",
                    [src],
                    [dst],
                    name=f"{node.name}_k5_{i}",
                    kernel_shape=[5, 5],
                    strides=[1, 1],
                    pads=[2, 2, 2, 2],
                )
            )
            src = dst


def prepare_graph(onnx_path: Path, out: Path) -> Path:
    """Write the copy of the graph the toolkit compiles: constants folded by
    ONNX Runtime's basic (standard-op only) graph optimisation, so the in-graph
    box decoder's shape arithmetic reaches the toolkit as constants; large
    spatial-pyramid max-pools as chained 5x5 pools (see ``_cascade_maxpools``);
    and every ``Resize`` naming its unused ``roi`` input. The PyTorch exporter
    leaves that input as the empty name, which the toolkit's constant folding
    cannot look up; an empty float initializer is the same graph to every
    runtime. Returns ``out``."""
    import onnx
    import onnxruntime as ort
    from onnx import TensorProto, helper

    # Pools first, on the exporter's graph: the optimiser rewrites node
    # attributes, and the match below expects the exporter's.
    pooled = out.with_suffix(".pools.onnx")
    model = onnx.load(str(onnx_path))
    _cascade_maxpools(model.graph)
    onnx.save(model, str(pooled))

    opts = ort.SessionOptions()
    opts.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_BASIC
    opts.optimized_model_filepath = str(out)
    ort.InferenceSession(str(pooled), opts, providers=["CPUExecutionProvider"])
    pooled.unlink()

    model = onnx.load(str(out))
    graph = model.graph
    roi = "ados_empty_roi"
    named = 0
    for node in graph.node:
        if node.op_type == "Resize" and len(node.input) > 1 and node.input[1] == "":
            node.input[1] = roi
            named += 1
    if named:
        graph.initializer.append(helper.make_tensor(roi, TensorProto.FLOAT, [0], []))
    onnx.checker.check_model(model)
    onnx.save(model, str(out))
    return out


def convert(
    onnx_path: Path,
    platform: str,
    out: Path,
    mean: list[float],
    std: list[float],
    tol: float,
) -> float:
    """Compile ``onnx_path`` for ``platform`` into ``out``, then run the graph
    in ONNX Runtime and the compiled model in the toolkit's simulator on three
    synthetic images (uniform noise, a colour gradient, blocks). Returns the
    largest output difference relative to each image's output range; above
    ``tol`` is an error."""
    import tempfile

    import numpy as np
    import onnxruntime as ort
    from rknn.api import RKNN

    name, h, w = _onnx_input(onnx_path)
    rng = np.random.default_rng(7)
    ys, xs = np.mgrid[0:h, 0:w]
    gradient = np.stack([xs * 255 // w, ys * 255 // h, (xs + ys) * 255 // (w + h)], -1)
    blocks = ((xs // 40 + ys // 40) % 2)[..., None] * np.array([200, 120, 40]) + 30
    frames = [
        rng.integers(0, 256, size=(1, h, w, 3), dtype=np.uint8),
        gradient[None].astype(np.uint8),
        blocks[None].astype(np.uint8),
    ]
    sess = ort.InferenceSession(str(onnx_path), providers=["CPUExecutionProvider"])
    m = np.asarray(mean, np.float32)
    s = np.asarray(std, np.float32)
    # Every output of every frame (a raw-heads model has one per stride).
    wants = [
        sess.run(None, {name: np.ascontiguousarray(((f - m) / s).transpose(0, 3, 1, 2))})
        for f in frames
    ]

    rknn = RKNN(verbose=False)
    try:
        with tempfile.TemporaryDirectory() as tmp:
            graph = prepare_graph(onnx_path, Path(tmp) / onnx_path.name)
            if rknn.config(mean_values=[mean], std_values=[std], target_platform=platform) != 0:
                raise SystemExit(f"rknn config failed for {platform}")
            if rknn.load_onnx(model=str(graph)) != 0:
                raise SystemExit(f"loading {onnx_path} failed")
            if rknn.build(do_quantization=False) != 0:
                raise SystemExit(f"building for {platform} failed")
        if rknn.export_rknn(str(out)) != 0:
            raise SystemExit(f"exporting {out} failed")
        if rknn.init_runtime() != 0:
            raise SystemExit(f"the {platform} simulator did not start")
        gots = [rknn.inference(inputs=[f], data_format="nhwc") for f in frames]
    finally:
        rknn.release()
    err = 0.0
    for want_outs, got_outs in zip(wants, gots, strict=True):
        for want, got in zip(want_outs, got_outs, strict=True):
            got = np.asarray(got, dtype=np.float32).reshape(want.shape)
            span = float(np.max(want) - np.min(want)) or 1.0
            err = max(err, float(np.max(np.abs(got - want))) / span)
    if err > tol:
        raise SystemExit(f"{platform}: simulator output diverges from ONNX ({err:.3e} > {tol})")
    return err


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--onnx", type=Path, required=True)
    ap.add_argument("--platform", action="append", choices=PLATFORMS, required=True)
    ap.add_argument("--out-dir", type=Path, required=True)
    ap.add_argument("--mean", type=float, nargs=3, default=[0.0, 0.0, 0.0])
    ap.add_argument("--std", type=float, nargs=3, default=[255.0, 255.0, 255.0])
    ap.add_argument(
        "--tolerance",
        type=float,
        default=0.01,
        help="largest simulator-vs-ONNX difference, as a share of the output range",
    )
    args = ap.parse_args()

    have = _toolkit_version()
    if have != TOOLKIT_VERSION:
        print(f"rknn-toolkit2 {have} found, {TOOLKIT_VERSION} required", file=sys.stderr)
        return 1
    _onnx_input(args.onnx)
    args.out_dir.mkdir(parents=True, exist_ok=True)
    for platform in dict.fromkeys(args.platform):
        out = args.out_dir / f"{args.onnx.stem}_{platform}.rknn"
        err = convert(args.onnx, platform, out, args.mean, args.std, args.tolerance)
        print(
            f"{out.name} platform {platform} size_bytes {out.stat().st_size} "
            f"sha256 {_sha256(out)} sim_rel_err {err:.3e}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
