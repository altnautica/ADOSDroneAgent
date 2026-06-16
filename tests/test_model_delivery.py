# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""Model-delivery framework: by-reference resolution (board-match select → verified cache
hit → fetch + sha256 verify → needs-model reason). Exercised with a generic detector,
network-free (cache / needs-model / verify-fail paths)."""

from __future__ import annotations

import asyncio
import hashlib
import json
from types import SimpleNamespace

from ados.plugins.manifest import VisionContribution, VisionModelRef
from ados.services.vision.model_manager import (
    ModelManager,
    ModelResolution,
    ResolutionState,
    board_family,
    registry_auth_headers,
    sha256_file,
)


def _mgr(tmp_path) -> ModelManager:
    cfg = SimpleNamespace(models_dir=str(tmp_path), registry_url="", models_cache_max_mb=100)
    return ModelManager(cfg)  # type: ignore[arg-type]


def _ref(**kw) -> VisionModelRef:
    base = dict(id="coco-detector", runtime="onnx", board_match="generic")
    base.update(kw)
    return VisionModelRef(**base)


# ── helpers ──────────────────────────────────────────────────────────────────
def test_board_family_mapping() -> None:
    assert board_family("rk3588s2") == "rk3588"
    assert board_family("rock-5c-lite") == "rk3588"
    assert board_family("orange-pi-5") == "rk3588"
    assert board_family("jetson-orin-nano") == "orin"
    assert board_family("generic-arm64") == "generic"
    assert board_family("rpi4b") == "generic"
    assert board_family(None) == "generic"


def test_sha256_file(tmp_path) -> None:
    f = tmp_path / "m.bin"
    f.write_bytes(b"weights")
    assert sha256_file(f) == hashlib.sha256(b"weights").hexdigest()


def test_select_ref_for_board(tmp_path) -> None:
    mgr = _mgr(tmp_path)
    refs = [_ref(runtime="rknn", board_match="rk3588"),
            _ref(runtime="tensorrt", board_match="orin"),
            _ref(runtime="onnx", board_match="generic")]
    assert mgr.select_ref_for_board(refs, "rk3588s2").runtime == "rknn"
    assert mgr.select_ref_for_board(refs, "jetson-orin-nano").runtime == "tensorrt"
    assert mgr.select_ref_for_board(refs, "rpi4b").runtime == "onnx"   # generic fallback


# ── resolution paths (network-free) ───────────────────────────────────────────
def test_resolve_cache_hit(tmp_path) -> None:
    mgr = _mgr(tmp_path)
    content = b"a-real-model-blob"
    cached = tmp_path / "coco-detector-rk3588.rknn"
    cached.write_bytes(content)
    ref = _ref(runtime="rknn", board_match="rk3588", sha256=hashlib.sha256(content).hexdigest())
    res = asyncio.run(mgr.resolve_model_ref([ref], "rk3588s2"))
    assert res.ok and res.state == ResolutionState.RESOLVED and res.path == str(cached)


def test_resolve_needs_model_when_absent_and_no_source(tmp_path) -> None:
    mgr = _mgr(tmp_path)
    ref = _ref(runtime="rknn", board_match="rk3588", sha256="deadbeef")  # nothing cached, no source
    res = asyncio.run(mgr.resolve_model_ref([ref], "rk3588s2"))
    assert res.state == ResolutionState.NEEDS_MODEL and res.reason and "not cached" in res.reason


def test_resolve_needs_model_when_fetch_disallowed(tmp_path) -> None:
    mgr = _mgr(tmp_path)
    ref = _ref(runtime="onnx", board_match="generic", sha256="deadbeef", source="https://example/m.onnx")
    res = asyncio.run(mgr.resolve_model_ref([ref], "rpi4b", allow_fetch=False))
    assert res.state == ResolutionState.NEEDS_MODEL


def test_resolve_verify_failed_on_bad_digest(tmp_path) -> None:
    mgr = _mgr(tmp_path)
    # a fetch that lands a file whose digest will NOT match the pinned sha256
    async def _bad_fetch(ref):
        p = tmp_path / "coco-detector-generic.onnx"
        p.write_bytes(b"corrupted-or-wrong-model")
        return p
    mgr._fetch_ref = _bad_fetch  # type: ignore[assignment]
    ref = _ref(runtime="onnx", board_match="generic",
               sha256="00" * 32, source="https://example/m.onnx")
    res = asyncio.run(mgr.resolve_model_ref([ref], "rpi4b"))
    assert res.state == ResolutionState.VERIFY_FAILED
    assert not (tmp_path / "coco-detector-generic.onnx").exists()  # bad file evicted


def test_resolve_bundled_path(tmp_path) -> None:
    mgr = _mgr(tmp_path)
    ref = _ref(runtime="onnx", board_match="generic", path="models/coco.onnx")  # bundled, no source
    res = asyncio.run(mgr.resolve_model_ref([ref], "rpi4b"))
    assert res.ok and res.path == "models/coco.onnx"


# ── manifest typed-view exerciser ──────────────────────────────────────────────
def test_manifest_model_refs_typed() -> None:
    vc = VisionContribution(models=[
        {"id": "coco-detector", "runtime": "rknn", "board_match": "rk3588",
         "sha256": "abc", "source": "registry://generic/coco"},
        {"id": "coco-detector", "runtime": "onnx", "path": "models/coco.onnx"},  # bundled
        {"not": "a-ref"},                                                        # skipped (no id/runtime)
    ])
    refs = vc.model_refs()
    assert len(refs) == 2
    assert refs[0].board_match == "rk3588" and refs[0].source == "registry://generic/coco"
    assert refs[1].path == "models/coco.onnx" and refs[1].source is None


def test_resolution_to_dict() -> None:
    d = ModelResolution(ResolutionState.NEEDS_MODEL, "coco-detector", "onnx", reason="x").to_dict()
    assert d["state"] == "needs_model" and d["model_id"] == "coco-detector" and d["reason"] == "x"


# ── board-family siblings + display names ──────────────────────────────────────
def test_board_family_siblings_and_display_names() -> None:
    # RK3588 NPU-class sibling (rk3582) + a display name carrying the SoC both resolve.
    assert board_family("rk3582") == "rk3588"
    assert board_family("Radxa ROCK 5C Lite (RK3582)") == "rk3588"
    assert board_family("NVIDIA Jetson Orin Nano") == "orin"
    assert board_family("tegra234") == "orin"
    # the small rk356x NPU is a different class → generic, not rk3588
    assert board_family("rk3566") == "generic"


# ── private-registry auth headers (network-free) ───────────────────────────────
def test_registry_auth_headers(tmp_path) -> None:
    creds = tmp_path / "model-registry-auth.json"
    creds.write_text(json.dumps({
        "models.example.com": "s3cr3t",                                  # bare token → Bearer
        "registry.example.org": "Bearer already-scheme",                 # scheme passthrough
        "custom.example.net": {"header": "X-Api-Key", "value": "k123"},  # custom header
    }))

    def h(url: str) -> dict:
        return registry_auth_headers(url, creds_path=creds)

    assert h("https://models.example.com/m.rknn") == {"Authorization": "Bearer s3cr3t"}
    assert h("https://registry.example.org/m.onnx") == {"Authorization": "Bearer already-scheme"}
    assert h("https://custom.example.net/m.onnx") == {"X-Api-Key": "k123"}
    assert h("https://unlisted.example.com/m.onnx") == {}                # host not listed
    # a signed-URL source whose creds file is absent → no header (credential is in the query)
    assert registry_auth_headers("https://x.com/m?sig=abc", creds_path=tmp_path / "absent.json") == {}


# ── multi-model plugin resolution (group-by-id) ────────────────────────────────
def test_resolve_plugin_models_groups_by_id(tmp_path) -> None:
    mgr = _mgr(tmp_path)
    content = b"detector-weights"
    (tmp_path / "det-a-generic.onnx").write_bytes(content)  # cached + verified
    refs = [
        _ref(id="det-a", runtime="onnx", board_match="generic",
             sha256=hashlib.sha256(content).hexdigest()),
        _ref(id="det-b", runtime="rknn", board_match="rk3588", sha256="deadbeef"),  # absent, no source
    ]
    out = asyncio.run(mgr.resolve_plugin_models(refs, "rk3588s2"))
    assert len(out) == 2  # one resolution per distinct id
    by_id = {r.model_id: r for r in out}
    assert by_id["det-a"].state == ResolutionState.RESOLVED
    assert by_id["det-b"].state == ResolutionState.NEEDS_MODEL
