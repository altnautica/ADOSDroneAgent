# Exempt from 300 LOC soft rule: one cohesive ML step (depth inference +
# back-projection + PLY writer) kept in a single readable module.
"""Monocular-depth back-projection seed for gaussian-splat reconstruction.

A gaussian-splat trainer (msplat / Brush) initializes from a `points3D.ply`
sitting next to a Nerfstudio `transforms.json`, in the SAME world frame as the
camera poses. A uniform-random cloud filling the camera-surrounded box is a
valid floor, but a cloud whose points sit on the ACTUAL surfaces the cameras
saw is a far better geometric prior: densification starts near the real
geometry instead of searching for it.

This module produces that cloud. For each keyframe it runs a monocular
metric-depth model, back-projects the depth map into world points through the
frame's intrinsics + camera-to-world pose, colors the points from the source
image, and writes one `points3D.ply` co-framed with `transforms.json`.

Two conventions matter and are handled explicitly:

- Intrinsics scale: `transforms.json` may declare intrinsics for a resolution
  that differs from the images on disk (e.g. declared 1957 wide while the
  images are 979 wide). The intrinsics are scaled to each image's real size.

- Camera basis: a Nerfstudio `transform_matrix` is camera-to-world in the
  OpenGL/Blender basis (camera looks down -Z, +Y up). A pinhole back-projection
  is naturally in the OpenCV basis (+Z forward, +Y down). The camera-space
  point is converted OpenCV -> OpenGL with `diag(1, -1, -1)` before the pose
  rotates it into the world, so the cloud lands in the poses' frame.

Metric depth has an unknown global scale relative to the (arbitrary) world
scale of the poses, so each frame's depth is rescaled to the geometric
look-at distance from that camera to the scene centroid. This anchors every
frame to one shared reference, which is what makes the per-frame clouds line
up in world space.

The model (Depth-Anything-V2 **Small**, Apache-2.0 upstream; NOT the
Base/Large/Giant variants which are CC-BY-NC) is loaded lazily so importing
this module needs only numpy + Pillow. `--check` verifies the ML stack is
importable without loading a model, so a caller can decide depth-vs-random
seeding cheaply and fall back cleanly when the stack is absent.
"""

from __future__ import annotations

import argparse
import json
import struct
import sys
import time
from pathlib import Path

import numpy as np

# Apache-2.0 metric Small model (24.8M params). Overridable, but the default is
# deliberately the permissively-licensed Small variant.
DEFAULT_MODEL = "depth-anything/Depth-Anything-V2-Metric-Indoor-Small-hf"
# Total point budget across all frames (the per-frame sample count is derived
# from this and the frame count). Enough to give densification a dense start
# without bloating trainer init.
DEFAULT_BUDGET = 200_000
# Longest side the image is resized to for inference. Depth-Anything runs at
# multiples of 14; 518 is the model's native size and a good speed/quality point.
INFER_LONG_SIDE = 518
# Keep points whose rescaled depth lands within this band of the look-at
# distance, dropping sky/background (too far) and near-lens noise (too close).
NEAR_FACTOR = 0.1
FAR_FACTOR = 4.0

POINTS_FILE = "points3D.ply"
TRANSFORMS_FILE = "transforms.json"


class DepthSeedError(RuntimeError):
    """A depth seed could not be produced; the caller should fall back to random."""


def _import_ml():
    """Import torch + transformers lazily. Raises DepthSeedError if unavailable."""
    try:
        import torch  # noqa: F401
        from transformers import AutoImageProcessor, AutoModelForDepthEstimation  # noqa: F401
    except Exception as exc:  # pragma: no cover - environment dependent
        raise DepthSeedError(f"ML stack unavailable: {exc}") from exc
    return torch, AutoImageProcessor, AutoModelForDepthEstimation


def _pick_device(torch) -> str:
    """The best available accelerator: MPS (Apple Silicon), CUDA, else CPU."""
    if torch.backends.mps.is_available():
        return "mps"
    if torch.cuda.is_available():
        return "cuda"
    return "cpu"


class DepthModel:
    """A loaded metric-depth model that maps an RGB image to a metric depth map."""

    def __init__(self, model_id: str, device: str, torch, image_processor, model):
        self.model_id = model_id
        self.device = device
        self._torch = torch
        self._proc = image_processor
        self._model = model

    @classmethod
    def load(cls, model_id: str) -> DepthModel:
        torch, AutoImageProcessor, AutoModelForDepthEstimation = _import_ml()
        device = _pick_device(torch)
        proc = AutoImageProcessor.from_pretrained(model_id)
        model = AutoModelForDepthEstimation.from_pretrained(model_id).to(device).eval()
        return cls(model_id, device, torch, proc, model)

    def infer(self, rgb: np.ndarray) -> np.ndarray:
        """Metric depth (meters, float32 HxW) for an HxWx3 uint8 RGB image."""
        from PIL import Image

        torch = self._torch
        h, w = rgb.shape[:2]
        img = Image.fromarray(rgb)
        inputs = self._proc(images=img, return_tensors="pt")
        inputs = {k: v.to(self.device) for k, v in inputs.items()}
        with torch.no_grad():
            out = self._model(**inputs)
        # Resize the predicted depth back to the source image size. Prefer the
        # processor's own post-process (handles the model's padding/scaling);
        # fall back to a plain bilinear resize if it is not present.
        depth = None
        post = getattr(self._proc, "post_process_depth_estimation", None)
        if callable(post):
            try:
                res = post(out, target_sizes=[(h, w)])
                depth = res[0]["predicted_depth"]
            except Exception:
                depth = None
        if depth is None:
            pred = out.predicted_depth
            if pred.dim() == 3:
                pred = pred.unsqueeze(1)
            depth = torch.nn.functional.interpolate(
                pred, size=(h, w), mode="bilinear", align_corners=False
            )[0, 0]
        return depth.detach().to("cpu").float().numpy()


def _load_image(path: Path) -> np.ndarray:
    """Load an image as an HxWx3 uint8 RGB array."""
    from PIL import Image

    with Image.open(path) as im:
        return np.asarray(im.convert("RGB"), dtype=np.uint8)


def _frame_intrinsics(manifest: dict, frame: dict, img_w: int, img_h: int):
    """(fx, fy, cx, cy) scaled from the manifest's declared resolution to the
    real image size. Per-frame intrinsics override the manifest globals when
    present (some Nerfstudio manifests carry both)."""

    def pick(key, default):
        v = frame.get(key)
        if v is None:
            v = manifest.get(key, default)
        return float(v)

    decl_w = float(frame.get("w", manifest.get("w", img_w)))
    decl_h = float(frame.get("h", manifest.get("h", img_h)))
    sx = img_w / decl_w if decl_w > 0 else 1.0
    sy = img_h / decl_h if decl_h > 0 else 1.0
    fx = pick("fl_x", img_w) * sx
    fy = pick("fl_y", img_h) * sy
    cx = pick("cx", img_w / 2.0) * sx
    cy = pick("cy", img_h / 2.0) * sy
    return fx, fy, cx, cy


def _camera_centers(frames: list) -> np.ndarray:
    """Nx3 camera centers (the pose translation column), invariant to the
    OpenGL/OpenCV basis so the centroid is the same on either convention."""
    centers = []
    for f in frames:
        m = np.asarray(f["transform_matrix"], dtype=np.float64)
        centers.append(m[:3, 3])
    return np.asarray(centers, dtype=np.float64)


def _look_distance(c2w: np.ndarray, centroid: np.ndarray) -> float:
    """The geometric look-at distance: how far the scene centroid is along the
    camera's world forward axis (OpenGL forward = -Z column). Falls back to the
    straight-line distance for a camera not facing the centroid."""
    center = c2w[:3, 3]
    forward = -c2w[:3, 2]
    to_centroid = centroid - center
    along = float(np.dot(to_centroid, forward))
    straight = float(np.linalg.norm(to_centroid))
    if not np.isfinite(along) or along < 0.1 * straight:
        return max(straight, 1e-3)
    return along


def _backproject(depth: np.ndarray, us: np.ndarray, vs: np.ndarray, fx, fy, cx, cy,
                 c2w: np.ndarray) -> np.ndarray:
    """World points for pixels (us, vs) at their scaled depths. Pinhole
    back-projection in the OpenCV camera basis, converted OpenCV -> OpenGL with
    diag(1,-1,-1), then rotated into the world by the camera-to-world pose."""
    z = depth[vs, us].astype(np.float64)
    x_cv = (us.astype(np.float64) - cx) / fx * z
    y_cv = (vs.astype(np.float64) - cy) / fy * z
    # OpenCV camera (z forward, y down) -> OpenGL camera (z back, y up).
    cam = np.stack([x_cv, -y_cv, -z], axis=1)  # (N, 3)
    r = c2w[:3, :3]
    t = c2w[:3, 3]
    return cam @ r.T + t


def build_seed(dataset_dir: Path, out_path: Path, budget: int, model_id: str,
               verbose: bool = True) -> dict:
    """Back-project every keyframe's metric depth into one colored world cloud
    and write `out_path` (binary_little_endian PLY, xyz float + rgb uchar).
    Returns a summary dict."""
    manifest_path = dataset_dir / TRANSFORMS_FILE
    if not manifest_path.is_file():
        raise DepthSeedError(f"no manifest at {manifest_path}")
    manifest = json.loads(manifest_path.read_text())
    frames = manifest.get("frames") or []
    if len(frames) < 2:
        raise DepthSeedError(f"need >= 2 frames, found {len(frames)}")

    centers = _camera_centers(frames)
    centroid = centers.mean(axis=0)

    model = DepthModel.load(model_id)
    per_frame = max(1, budget // len(frames))

    all_pts: list[np.ndarray] = []
    all_rgb: list[np.ndarray] = []
    used_frames = 0
    t0 = time.time()

    for idx, frame in enumerate(frames):
        img_path = dataset_dir / frame["file_path"]
        if not img_path.is_file():
            # Some manifests omit the extension; try a couple of common ones.
            for ext in (".jpg", ".jpeg", ".png"):
                cand = img_path.with_suffix(ext)
                if cand.is_file():
                    img_path = cand
                    break
        if not img_path.is_file():
            if verbose:
                print(f"skip frame {idx}: no image at {img_path}", file=sys.stderr)
            continue

        try:
            rgb = _load_image(img_path)
        except Exception as exc:
            if verbose:
                print(f"skip frame {idx}: image load failed: {exc}", file=sys.stderr)
            continue

        h, w = rgb.shape[:2]
        # Run inference on a resized copy for speed, then map depth back to full
        # res so the intrinsics + colors line up at native resolution.
        infer_rgb = _resize_long_side(rgb, INFER_LONG_SIDE)
        depth_small = model.infer(infer_rgb)
        depth = _resize_to(depth_small, h, w)

        c2w = np.asarray(frame["transform_matrix"], dtype=np.float64)
        fx, fy, cx, cy = _frame_intrinsics(manifest, frame, w, h)

        # Per-frame metric->world scale: make the central-region median depth
        # equal the geometric look-at distance, anchoring the frame to the
        # shared centroid so all frames' clouds are mutually consistent.
        valid = np.isfinite(depth) & (depth > 1e-6)
        if valid.sum() < 16:
            continue
        cy0, cy1 = int(h * 0.25), int(h * 0.75)
        cx0, cx1 = int(w * 0.25), int(w * 0.75)
        central = depth[cy0:cy1, cx0:cx1]
        central = central[np.isfinite(central) & (central > 1e-6)]
        if central.size < 16:
            central = depth[valid]
        med = float(np.median(central))
        if med <= 1e-6:
            continue
        look = _look_distance(c2w, centroid)
        scale = look / med
        depth = depth * scale

        # Keep points within a band of the look-at distance (drop sky + near noise).
        near = NEAR_FACTOR * look
        far = FAR_FACTOR * look
        keep = valid & (depth > near) & (depth < far)
        ys, xs = np.nonzero(keep)
        if xs.size == 0:
            continue
        if xs.size > per_frame:
            sel = np.random.default_rng(idx).choice(xs.size, size=per_frame, replace=False)
            xs, ys = xs[sel], ys[sel]

        pts = _backproject(depth, xs, ys, fx, fy, cx, cy, c2w)
        cols = rgb[ys, xs].astype(np.uint8)
        all_pts.append(pts.astype(np.float32))
        all_rgb.append(cols)
        used_frames += 1
        if verbose and (idx % 25 == 0 or idx == len(frames) - 1):
            print(f"frame {idx + 1}/{len(frames)}: +{xs.size} pts "
                  f"(scale {scale:.3f}, look {look:.2f})", file=sys.stderr)

    if not all_pts:
        raise DepthSeedError("no frames produced points")

    xyz = np.concatenate(all_pts, axis=0)
    rgb_all = np.concatenate(all_rgb, axis=0)
    # Drop non-finite points (a numerically bad pose/depth) before capping.
    finite = np.isfinite(xyz).all(axis=1)
    xyz, rgb_all = xyz[finite], rgb_all[finite]
    if xyz.shape[0] > budget:
        sel = np.random.default_rng(0).choice(xyz.shape[0], size=budget, replace=False)
        xyz, rgb_all = xyz[sel], rgb_all[sel]

    _write_ply(out_path, xyz, rgb_all)
    return {
        "points": int(xyz.shape[0]),
        "frames_used": used_frames,
        "frames_total": len(frames),
        "model": model_id,
        "device": model.device,
        "seconds": round(time.time() - t0, 2),
        "out": str(out_path),
    }


def _resize_long_side(rgb: np.ndarray, long_side: int) -> np.ndarray:
    """Resize so the longest side is `long_side`, preserving aspect (uint8 RGB)."""
    from PIL import Image

    h, w = rgb.shape[:2]
    if max(h, w) <= long_side:
        return rgb
    scale = long_side / max(h, w)
    nw, nh = max(1, round(w * scale)), max(1, round(h * scale))
    im = Image.fromarray(rgb).resize((nw, nh), Image.BILINEAR)
    return np.asarray(im, dtype=np.uint8)


def _resize_to(depth: np.ndarray, h: int, w: int) -> np.ndarray:
    """Bilinear-resize a float depth map to (h, w)."""
    from PIL import Image

    if depth.shape == (h, w):
        return depth
    im = Image.fromarray(depth.astype(np.float32), mode="F").resize((w, h), Image.BILINEAR)
    return np.asarray(im, dtype=np.float32)


def _write_ply(path: Path, xyz: np.ndarray, rgb: np.ndarray) -> None:
    """Write a binary little-endian PLY: x/y/z float32 + red/green/blue uchar.
    The trainer's PLY reader keys on property names, so the colors seed the
    initial gaussian colors; a reader that ignores them still reads xyz."""
    n = xyz.shape[0]
    header = (
        "ply\n"
        "format binary_little_endian 1.0\n"
        f"element vertex {n}\n"
        "property float x\n"
        "property float y\n"
        "property float z\n"
        "property uchar red\n"
        "property uchar green\n"
        "property uchar blue\n"
        "end_header\n"
    ).encode("ascii")
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "wb") as f:
        f.write(header)
        # Interleave xyz (float32) + rgb (uint8) per vertex.
        xyz32 = xyz.astype("<f4", copy=False)
        rgb8 = rgb.astype("<u1", copy=False)
        buf = bytearray()
        pack = struct.Struct("<fffBBB").pack
        for i in range(n):
            buf += pack(
                float(xyz32[i, 0]), float(xyz32[i, 1]), float(xyz32[i, 2]),
                int(rgb8[i, 0]), int(rgb8[i, 1]), int(rgb8[i, 2]),
            )
        f.write(buf)


def _cmd_check() -> int:
    """Verify the ML stack imports (fast, no model load). Prints ok/reason."""
    try:
        _import_ml()
    except DepthSeedError as exc:
        print(str(exc))
        return 1
    print("ok")
    return 0


def main(argv: list | None = None) -> int:
    parser = argparse.ArgumentParser(description="Monocular-depth back-projection seed.")
    parser.add_argument("dataset_dir", nargs="?", help="dir holding transforms.json + images/")
    parser.add_argument("--out", default=None, help="output PLY (default <dataset_dir>/points3D.ply)")
    parser.add_argument("--budget", type=int, default=DEFAULT_BUDGET, help="total point budget")
    parser.add_argument("--model", default=DEFAULT_MODEL, help="HF metric-depth model id")
    parser.add_argument("--check", action="store_true", help="verify the ML stack, no model load")
    parser.add_argument("--quiet", action="store_true", help="suppress progress on stderr")
    args = parser.parse_args(argv)

    if args.check:
        return _cmd_check()
    if not args.dataset_dir:
        parser.error("dataset_dir is required unless --check")

    dataset_dir = Path(args.dataset_dir)
    out_path = Path(args.out) if args.out else dataset_dir / POINTS_FILE
    try:
        summary = build_seed(dataset_dir, out_path, args.budget, args.model, verbose=not args.quiet)
    except DepthSeedError as exc:
        print(json.dumps({"ok": False, "error": str(exc)}))
        return 1
    print(json.dumps({"ok": True, **summary}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
