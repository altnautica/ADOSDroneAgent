#!/usr/bin/env python3
"""Pin the bundled plugin catalog to published ADOSExtensions releases.

Every entry in ``src/ados/data/plugin-catalog.json`` pins the latest published
GitHub release of one extension in the ADOSExtensions publish allowlist. The
allowlist is the set of ``<ext>-v*`` tag triggers in that repository's
``.github/workflows/*release*.yml``; an extension that is not listed there is
not published, so it is not in the catalog.

For each allowlisted extension the script takes the newest non-draft,
non-prerelease ``<ext>-v<semver>`` release that carries a
``<id>-<version>.signed.adosplug`` asset, then:

- ``archive_sha256`` from the asset's API ``digest``, cross-checked against the
  release's ``.sha256`` asset when one is attached (either alone is enough);
- ``download_url`` from the release download path;
- ``version``, ``description`` and ``risk`` from the manifest at that tag.

``name``, ``author``, ``homepage``, ``license``, ``category`` and ``halves`` of
an existing entry are kept. A new entry takes them from the manifest, with the
category from ``CATEGORIES``.

    python scripts/sync-plugin-catalog.py           # rewrite the catalog
    python scripts/sync-plugin-catalog.py --check   # exit 1 when it lags

Release data comes from ``gh api`` (authenticated through ``gh auth`` or
``GH_TOKEN``).
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any, Protocol

import yaml

REPO = Path(__file__).resolve().parent.parent
CATALOG = REPO / "src" / "ados" / "data" / "plugin-catalog.json"

EXTENSIONS_REPO = "altnautica/ADOSExtensions"
DOWNLOAD_BASE = f"https://github.com/{EXTENSIONS_REPO}/releases/download/"
ARCHIVE_SUFFIX = ".signed.adosplug"

# Extension directories never pinned, whatever the allowlist says. The FLIR
# Lepton driver has no capture backend, so no configuration of it produces a
# reading; it stays out until it is publishable.
EXCLUDED = {"thermal-camera-flir-lepton-usb"}

# The oldest release of an extension the catalog may pin. Vision-nav releases
# before 0.5.0 ignore the operator's configuration, so an install of one is
# worse than no entry; until 0.5.0 is published the catalog omits vision-nav.
MIN_VERSIONS = {"vision-nav": "0.5.0"}

# Catalog category of an extension, used when a new entry is added. Manifests
# carry no category field.
CATEGORIES = {
    "com.altnautica.mavlink-gimbal-v2": "drivers",
    "com.altnautica.siyi-pod": "drivers",
    "com.altnautica.follow-me": "ai",
    "com.altnautica.vision-nav": "ai",
    "com.altnautica.world-engine": "ai",
}

# Field order of a catalog entry, as the agent and the installer read it.
ENTRY_FIELDS = (
    "id",
    "name",
    "version",
    "description",
    "author",
    "homepage",
    "license",
    "risk",
    "category",
    "halves",
    "download_url",
    "archive_sha256",
)

SEMVER = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")


class SyncError(RuntimeError):
    """A release or manifest that cannot be pinned as published."""


class Source(Protocol):
    """Where release data comes from: ``gh api`` live, a fixture in tests."""

    def release_workflows(self) -> dict[str, str]: ...

    def releases(self) -> list[dict[str, Any]]: ...

    def manifest(self, ext: str, tag: str) -> str: ...

    def asset_text(self, asset: dict[str, Any]) -> str: ...


def parse_version(text: str) -> tuple[int, int, int] | None:
    m = SEMVER.match(text)
    return (int(m.group(1)), int(m.group(2)), int(m.group(3))) if m else None


def allowlist(workflows: dict[str, str]) -> list[str]:
    """Extension directories the release workflows publish, in first-seen order.

    Each ``on.push.tags`` pattern of the form ``<ext>-v*`` names one extension.
    """
    out: list[str] = []
    for name in sorted(workflows):
        doc = yaml.safe_load(workflows[name]) or {}
        # YAML 1.1 reads the bare key `on` as the boolean true.
        triggers = doc.get("on", doc.get(True)) or {}
        push = triggers.get("push") if isinstance(triggers, dict) else None
        tags = (push or {}).get("tags") or []
        for pattern in tags:
            if not isinstance(pattern, str) or not pattern.endswith("-v*"):
                raise SyncError(f"{name}: tag pattern {pattern!r} is not of the form <ext>-v*")
            ext = pattern[: -len("-v*")]
            if ext not in out:
                out.append(ext)
    return out


def archive_asset(release: dict[str, Any], version: str) -> dict[str, Any] | None:
    """The release's signed archive for ``version``, or None when it has none."""
    suffix = f"-{version}{ARCHIVE_SUFFIX}"
    found = [a for a in release.get("assets") or [] if a.get("name", "").endswith(suffix)]
    if len(found) > 1:
        names = ", ".join(a["name"] for a in found)
        raise SyncError(f"{release['tag_name']}: more than one archive ({names})")
    return found[0] if found else None


def latest_release(
    releases: list[dict[str, Any]], ext: str, min_version: str | None = None
) -> tuple[dict[str, Any], str, dict[str, Any]] | None:
    """The newest published ``<ext>-v<semver>`` release with a signed archive.

    Returns ``(release, version, archive_asset)``, or None when no release
    qualifies. Drafts, prereleases, non-semver tags and releases older than
    ``min_version`` are skipped.
    """
    prefix = f"{ext}-v"
    floor = parse_version(min_version) if min_version else None
    best: tuple[tuple[int, int, int], dict[str, Any], str, dict[str, Any]] | None = None
    for release in releases:
        tag = release.get("tag_name", "")
        if release.get("draft") or release.get("prerelease") or not tag.startswith(prefix):
            continue
        version = tag[len(prefix) :]
        key = parse_version(version)
        if key is None or (floor is not None and key < floor):
            continue
        asset = archive_asset(release, version)
        if asset is None:
            continue
        if best is None or key > best[0]:
            best = (key, release, version, asset)
    return (best[1], best[2], best[3]) if best else None


def digest_hex(asset: dict[str, Any]) -> str | None:
    """The sha256 the API records for an asset (``digest: sha256:<hex>``)."""
    digest = asset.get("digest") or ""
    if not digest.startswith("sha256:"):
        return None
    value = digest[len("sha256:") :].lower()
    return value if SHA256.match(value) else None


def sidecar_hex(text: str) -> str:
    """The sha256 in a ``.sha256`` file (``<hex>`` or ``<hex>  <name>``)."""
    token = text.strip().split()[0].lower() if text.strip() else ""
    if not SHA256.match(token):
        raise SyncError(f"sha256 file does not start with a sha256: {text[:80]!r}")
    return token


def archive_sha256(release: dict[str, Any], asset: dict[str, Any], source: Source) -> str:
    """The archive's sha256 from the API digest and/or the ``.sha256`` asset."""
    sidecar_name = asset["name"] + ".sha256"
    sidecar = next((a for a in release.get("assets") or [] if a.get("name") == sidecar_name), None)
    api = digest_hex(asset)
    file = sidecar_hex(source.asset_text(sidecar)) if sidecar else None
    if api and file and api != file:
        raise SyncError(f"{release['tag_name']}: API digest {api} != {sidecar_name} {file}")
    sha = api or file
    if sha is None:
        raise SyncError(
            f"{release['tag_name']}: {asset['name']} has no digest and no .sha256 asset"
        )
    return sha


def build_entry(
    existing: dict[str, Any] | None,
    manifest: dict[str, Any],
    tag: str,
    asset_name: str,
    sha: str,
) -> dict[str, Any]:
    """One catalog entry pinned to ``tag``."""
    plugin_id = manifest["id"]
    if existing is not None:
        base = dict(existing)
    else:
        category = CATEGORIES.get(plugin_id)
        if category is None:
            raise SyncError(f"{plugin_id}: new catalog entry needs a category in CATEGORIES")
        base = {
            "name": manifest["name"],
            "author": manifest.get("author", ""),
            "homepage": manifest.get("homepage", ""),
            "license": manifest.get("license", ""),
            "category": category,
            "halves": [h for h in ("agent", "gcs") if manifest.get(h)],
        }
    base.update(
        id=plugin_id,
        version=str(manifest["version"]),
        description=manifest["description"],
        risk=manifest["risk"],
        download_url=f"{DOWNLOAD_BASE}{tag}/{asset_name}",
        archive_sha256=sha,
    )
    ordered = {k: base[k] for k in ENTRY_FIELDS if k in base}
    ordered.update({k: v for k, v in base.items() if k not in ordered})
    return ordered


def pin(
    ext: str,
    existing_by_id: dict[str, dict[str, Any]],
    source: Source,
    releases: list[dict[str, Any]],
) -> dict[str, Any] | None:
    """The pinned entry for one allowlisted extension, or None when unpublished."""
    found = latest_release(releases, ext, MIN_VERSIONS.get(ext))
    if found is None:
        return None
    release, version, asset = found
    tag = release["tag_name"]
    manifest = yaml.safe_load(source.manifest(ext, tag)) or {}
    if str(manifest.get("version")) != version:
        raise SyncError(f"{tag}: manifest version {manifest.get('version')!r} != tag version")
    expected = f"{manifest.get('id')}-{version}{ARCHIVE_SUFFIX}"
    if asset["name"] != expected:
        raise SyncError(f"{tag}: archive {asset['name']!r}, expected {expected!r}")
    sha = archive_sha256(release, asset, source)
    return build_entry(existing_by_id.get(manifest["id"]), manifest, tag, asset["name"], sha)


def sync(catalog: dict[str, Any], source: Source) -> tuple[dict[str, Any], list[str]]:
    """The catalog pinned to the latest published releases, plus change notes.

    Entries keep their order; new ones are appended in allowlist order. An
    entry whose extension is excluded, not allowlisted, or has no qualifying
    release is dropped.
    """
    existing = list(catalog.get("plugins") or [])
    existing_by_id = {e["id"]: e for e in existing}
    releases = source.releases()
    pinned: dict[str, dict[str, Any]] = {}
    notes: list[str] = []
    for ext in allowlist(source.release_workflows()):
        if ext in EXCLUDED:
            notes.append(f"{ext}: excluded")
            continue
        entry = pin(ext, existing_by_id, source, releases)
        if entry is None:
            floor = MIN_VERSIONS.get(ext)
            notes.append(f"{ext}: no published release" + (f" >= {floor}" if floor else ""))
            continue
        pinned[entry["id"]] = entry
    plugins = [pinned[e["id"]] for e in existing if e["id"] in pinned]
    plugins += [e for pid, e in pinned.items() if pid not in existing_by_id]
    for old in existing:
        new = pinned.get(old["id"])
        if new is None:
            notes.append(f"{old['id']}: removed (was {old.get('version')})")
        elif new != old:
            notes.append(f"{old['id']}: {old.get('version')} -> {new['version']}")
    for pid, e in pinned.items():
        if pid not in existing_by_id:
            notes.append(f"{pid}: added at {e['version']}")
    out = {k: v for k, v in catalog.items() if k != "plugins"}
    out["plugins"] = plugins
    return out, notes


def render(catalog: dict[str, Any]) -> str:
    """The catalog as committed: two-space JSON with string lists on one line."""
    text = json.dumps(catalog, indent=2, ensure_ascii=False)
    text = re.sub(
        r"\[\n\s+(\"[^\"\n]*\"(?:,\n\s+\"[^\"\n]*\")*)\n\s+\]",
        lambda m: "[" + ", ".join(s.strip() for s in m.group(1).split(",\n")) + "]",
        text,
    )
    return text + "\n"


class GhSource:
    """Release data read live through ``gh api``."""

    def __init__(self, repo: str = EXTENSIONS_REPO) -> None:
        self.repo = repo

    def _api(self, path: str, *, accept: str | None = None, paginate: bool = False) -> str:
        cmd = ["gh", "api"]
        if paginate:
            cmd.append("--paginate")
        if accept:
            cmd += ["-H", f"Accept: {accept}"]
        cmd.append(path)
        done = subprocess.run(cmd, capture_output=True, text=True, encoding="utf-8")
        if done.returncode != 0:
            raise SyncError(f"gh api {path}: {done.stderr.strip()}")
        return done.stdout

    def release_workflows(self) -> dict[str, str]:
        listing = json.loads(self._api(f"repos/{self.repo}/contents/.github/workflows"))
        return {
            item["name"]: self._api(item["url"], accept="application/vnd.github.raw")
            for item in listing
            if "release" in item["name"] and item["name"].endswith((".yml", ".yaml"))
        }

    def releases(self) -> list[dict[str, Any]]:
        # --paginate concatenates one JSON array per page.
        raw = self._api(f"repos/{self.repo}/releases?per_page=100", paginate=True)
        out: list[dict[str, Any]] = []
        decoder = json.JSONDecoder()
        pos = 0
        while pos < len(raw):
            while pos < len(raw) and raw[pos].isspace():
                pos += 1
            if pos >= len(raw):
                break
            page, pos = decoder.raw_decode(raw, pos)
            out.extend(page)
        return out

    def manifest(self, ext: str, tag: str) -> str:
        path = f"repos/{self.repo}/contents/extensions/{ext}/manifest.yaml?ref={tag}"
        return self._api(path, accept="application/vnd.github.raw")

    def asset_text(self, asset: dict[str, Any]) -> str:
        return self._api(asset["url"], accept="application/octet-stream")


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--check", action="store_true", help="exit 1 when the catalog lags")
    ap.add_argument("--catalog", type=Path, default=CATALOG, help=argparse.SUPPRESS)
    args = ap.parse_args(argv)

    current = json.loads(args.catalog.read_text(encoding="utf-8"))
    try:
        desired, notes = sync(current, GhSource())
    except SyncError as e:
        print(f"sync-plugin-catalog: {e}", file=sys.stderr)
        return 2
    for note in notes:
        print(note)
    if args.check:
        if desired != current:
            print(
                "FAIL: the bundled plugin catalog lags the published ADOSExtensions "
                "releases. Run scripts/sync-plugin-catalog.py and commit the result.",
                file=sys.stderr,
            )
            return 1
        print("plugin catalog is current")
        return 0
    text = render(desired)
    if args.catalog.read_text(encoding="utf-8") != text:
        args.catalog.write_text(text, encoding="utf-8", newline="\n")
        print(f"wrote {args.catalog}")
    else:
        print("plugin catalog unchanged")
    return 0


if __name__ == "__main__":
    sys.exit(main())
