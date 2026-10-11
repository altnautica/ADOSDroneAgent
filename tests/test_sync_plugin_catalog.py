"""scripts/sync-plugin-catalog.py: pins against fixture release data, no network."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
from typing import Any

import pytest

_SCRIPT = Path(__file__).resolve().parent.parent / "scripts" / "sync-plugin-catalog.py"
_spec = importlib.util.spec_from_file_location("sync_plugin_catalog", _SCRIPT)
assert _spec is not None and _spec.loader is not None
sync_mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(sync_mod)

SHA_A = "a" * 64
SHA_B = "b" * 64
SHA_C = "c" * 64

RELEASE_YML = """\
name: release
on:
  push:
    tags:
      - "gimbal-v*"
      - "pod-v*"
jobs: {}
"""

VN_RELEASE_YML = """\
name: vision-nav-release
on:
  push:
    tags:
      - "vision-nav-v*"
jobs: {}
"""

MANIFESTS = {
    "gimbal": {
        "1.1.0": 'id: com.altnautica.mavlink-gimbal-v2\nname: Gimbal\nversion: "1.1.0"\n'
        "description: New gimbal text.\nrisk: medium\nagent: {runtime: python}\n"
        "gcs: {entrypoint: x.js}\n",
    },
    "pod": {
        "0.5.0": 'id: com.altnautica.siyi-pod\nname: "ADOS SIYI Optical Pod"\nversion: "0.5.0"\n'
        "description: Pod driver.\nauthor: Altnautica\nhomepage: https://example.com/pod\n"
        "license: GPL-3.0-or-later\nrisk: high\nagent: {runtime: python}\n"
        "gcs: {entrypoint: x.js}\n",
    },
    "vision-nav": {
        "0.4.1": 'id: com.altnautica.vision-nav\nname: VN\nversion: "0.4.1"\n'
        "description: Old.\nrisk: high\nagent: {}\ngcs: {}\n",
    },
}


def asset(name: str, digest: str | None = None) -> dict[str, Any]:
    out: dict[str, Any] = {"name": name, "url": f"https://api.example.com/assets/{name}"}
    if digest is not None:
        out["digest"] = f"sha256:{digest}"
    return out


def release(
    tag: str, *assets: dict[str, Any], draft: bool = False, pre: bool = False
) -> dict[str, Any]:
    return {"tag_name": tag, "draft": draft, "prerelease": pre, "assets": list(assets)}


GIMBAL = "com.altnautica.mavlink-gimbal-v2"
POD = "com.altnautica.siyi-pod"


def releases() -> list[dict[str, Any]]:
    return [
        release("gimbal-v1.0.2", asset(f"{GIMBAL}-1.0.2.signed.adosplug", SHA_C)),
        release("gimbal-v1.1.0", asset(f"{GIMBAL}-1.1.0.signed.adosplug", SHA_A)),
        release("gimbal-v1.2.0", asset(f"{GIMBAL}-1.2.0.signed.adosplug", SHA_C), draft=True),
        release("gimbal-v1.3.0", asset(f"{GIMBAL}-1.3.0.signed.adosplug", SHA_C), pre=True),
        # Digest absent: the .sha256 sidecar carries the pin.
        release(
            "pod-v0.5.0",
            asset(f"{POD}-0.5.0.signed.adosplug"),
            asset(f"{POD}-0.5.0.signed.adosplug.sha256"),
        ),
        # Old tag scheme and a release below the floor: never pinned.
        release(
            "extensions/vision-nav-v0.2.8",
            asset("com.altnautica.vision-nav-0.2.8.signed.adosplug", SHA_C),
        ),
        release(
            "vision-nav-v0.4.1", asset("com.altnautica.vision-nav-0.4.1.signed.adosplug", SHA_C)
        ),
    ]


class FakeSource:
    def __init__(
        self, rels: list[dict[str, Any]] | None = None, sidecars: dict[str, str] | None = None
    ):
        self._releases = releases() if rels is None else rels
        self._sidecars = sidecars or {
            f"{POD}-0.5.0.signed.adosplug.sha256": f"{SHA_B}  {POD}-0.5.0.signed.adosplug\n"
        }

    def release_workflows(self) -> dict[str, str]:
        return {"release.yml": RELEASE_YML, "vision-nav-release.yml": VN_RELEASE_YML}

    def releases(self) -> list[dict[str, Any]]:
        return self._releases

    def manifest(self, ext: str, tag: str) -> str:
        return MANIFESTS[ext][tag.rsplit("-v", 1)[1]]

    def asset_text(self, a: dict[str, Any]) -> str:
        return self._sidecars[a["name"]]


def catalog() -> dict[str, Any]:
    return {
        "schema_version": 1,
        "source": "first-party-bundled",
        "plugins": [
            {
                "id": "com.altnautica.thermal-flir-lepton-usb",
                "name": "Thermal",
                "version": "1.0.2",
                "download_url": "x",
                "archive_sha256": SHA_C,
            },
            {
                "id": GIMBAL,
                "name": "Kept Name",
                "version": "1.0.2",
                "description": "Old text.",
                "author": "Kept",
                "homepage": "https://example.com/kept",
                "license": "MIT",
                "risk": "low",
                "category": "drivers",
                "halves": ["agent"],
                "download_url": "old",
                "archive_sha256": SHA_C,
            },
            {"id": "com.altnautica.vision-nav", "name": "VN", "version": "0.2.8"},
        ],
    }


@pytest.fixture(autouse=True)
def _floor(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(sync_mod, "MIN_VERSIONS", {"vision-nav": "0.5.0"})
    monkeypatch.setattr(sync_mod, "CATEGORIES", {POD: "drivers"})


def test_allowlist_reads_tag_patterns_from_every_release_workflow() -> None:
    wf = FakeSource().release_workflows()
    assert sync_mod.allowlist(wf) == ["gimbal", "pod", "vision-nav"]


def test_allowlist_rejects_a_pattern_it_cannot_map() -> None:
    with pytest.raises(sync_mod.SyncError):
        sync_mod.allowlist({"release.yml": 'on:\n  push:\n    tags: ["v*"]\n'})


def test_latest_release_skips_drafts_prereleases_and_other_schemes() -> None:
    rel, version, a = sync_mod.latest_release(releases(), "gimbal")
    assert (rel["tag_name"], version, a["name"]) == (
        "gimbal-v1.1.0",
        "1.1.0",
        f"{GIMBAL}-1.1.0.signed.adosplug",
    )


def test_latest_release_honours_the_floor() -> None:
    assert sync_mod.latest_release(releases(), "vision-nav", "0.5.0") is None
    assert sync_mod.latest_release(releases(), "vision-nav")[1] == "0.4.1"


def test_sha_comes_from_digest_or_sidecar_and_they_must_agree() -> None:
    src = FakeSource()
    rel = release(
        "x-v1.0.0", asset("p-1.0.0.signed.adosplug", SHA_A), asset("p-1.0.0.signed.adosplug.sha256")
    )
    src._sidecars = {"p-1.0.0.signed.adosplug.sha256": SHA_A + "\n"}
    assert sync_mod.archive_sha256(rel, rel["assets"][0], src) == SHA_A
    src._sidecars = {"p-1.0.0.signed.adosplug.sha256": SHA_B + "\n"}
    with pytest.raises(sync_mod.SyncError):
        sync_mod.archive_sha256(rel, rel["assets"][0], src)
    bare = release("x-v1.0.0", asset("p-1.0.0.signed.adosplug"))
    with pytest.raises(sync_mod.SyncError):
        sync_mod.archive_sha256(bare, bare["assets"][0], src)


def test_sync_pins_keeps_curated_fields_adds_and_drops() -> None:
    out, notes = sync_mod.sync(catalog(), FakeSource())
    plugins = out["plugins"]
    assert [p["id"] for p in plugins] == [GIMBAL, POD]

    gimbal = plugins[0]
    assert gimbal["version"] == "1.1.0"
    assert gimbal["description"] == "New gimbal text."
    assert gimbal["risk"] == "medium"
    assert gimbal["archive_sha256"] == SHA_A
    assert gimbal["download_url"] == (
        "https://github.com/altnautica/ADOSExtensions/releases/download/"
        f"gimbal-v1.1.0/{GIMBAL}-1.1.0.signed.adosplug"
    )
    assert (gimbal["name"], gimbal["author"], gimbal["license"], gimbal["halves"]) == (
        "Kept Name",
        "Kept",
        "MIT",
        ["agent"],
    )

    pod = plugins[1]
    assert list(pod) == list(sync_mod.ENTRY_FIELDS)
    assert pod["archive_sha256"] == SHA_B
    assert (pod["category"], pod["halves"], pod["risk"]) == ("drivers", ["agent", "gcs"], "high")

    assert out["schema_version"] == 1 and out["source"] == "first-party-bundled"
    joined = "\n".join(notes)
    assert "thermal-flir-lepton-usb: removed" in joined
    assert "com.altnautica.vision-nav: removed" in joined
    assert "vision-nav: no published release >= 0.5.0" in joined


def test_sync_is_idempotent() -> None:
    once, _ = sync_mod.sync(catalog(), FakeSource())
    twice, notes = sync_mod.sync(json.loads(json.dumps(once)), FakeSource())
    assert twice == once
    assert notes == ["vision-nav: no published release >= 0.5.0"]


def test_manifest_version_must_match_the_tag() -> None:
    rels = [release("gimbal-v1.0.2", asset(f"{GIMBAL}-1.0.2.signed.adosplug", SHA_A))]
    src = FakeSource(rels)
    src.manifest = lambda ext, tag: MANIFESTS["gimbal"]["1.1.0"]  # type: ignore[method-assign]
    with pytest.raises(sync_mod.SyncError):
        sync_mod.sync(catalog(), src)


def test_new_entry_without_a_category_is_refused(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(sync_mod, "CATEGORIES", {})
    with pytest.raises(sync_mod.SyncError):
        sync_mod.sync(catalog(), FakeSource())


def test_render_keeps_string_lists_on_one_line() -> None:
    out, _ = sync_mod.sync(catalog(), FakeSource())
    text = sync_mod.render(out)
    assert '"halves": ["agent", "gcs"]' in text
    assert json.loads(text) == out
    assert text.endswith("}\n")


def test_committed_catalog_is_in_rendered_form() -> None:
    committed = sync_mod.CATALOG.read_text(encoding="utf-8")
    assert sync_mod.render(json.loads(committed)) == committed
