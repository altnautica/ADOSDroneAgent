"""``.adosplug`` archive packer and unpacker.

Archive layout (zip, no compression for binary stability):

    manifest.yaml                   required
    SIGNATURE                       optional, format below
    agent/                          optional, agent half
        wheel/<plugin>-<ver>-py3-none-any.whl
        src/                         OR loose Python source root (src-layout)
        py/                         OR loose Python source root (flat)
    gcs/                            optional, GCS half
        plugin.bundle.js
        style.css
        locale/<lang>.json
    assets/                         optional, additional files (models, fixtures)

SIGNATURE format:

    line 1: signer-id
    line 2: base64 ed25519 signature over sha256(canonical_payload)

Canonical payload is sha256-of-sorted-list-of (path, sha256_of_bytes)
across every entry except SIGNATURE itself. Sorting by path makes the
signing payload deterministic regardless of zip ordering.

The archive size limit is 50MB. The per-entry size limit is 25MB. Both
fail at unpack with :class:`ArchiveError`. Path traversal entries
(``..`` segments, absolute paths, symlinks) are rejected.
"""

from __future__ import annotations

import fnmatch
import hashlib
import io
import os
import zipfile
from dataclasses import dataclass
from pathlib import Path

from ados.core.logging import get_logger
from ados.plugins.errors import ArchiveError, ManifestError, SignatureError
from ados.plugins.manifest import PluginManifest

log = get_logger("plugins.archive")

ARCHIVE_MAX_BYTES = 50 * 1024 * 1024
ENTRY_MAX_BYTES = 25 * 1024 * 1024
SIGNATURE_FILENAME = "SIGNATURE"
MANIFEST_FILENAME = "manifest.yaml"

# Build, source-tree, test, and cache cruft that must never ride in a
# shipped archive. A shipped plugin carries the built GCS bundle
# (``gcs/plugin.bundle.js``) and the Python agent half under
# ``agent/<pkg>/`` — never the GCS TypeScript source, the npm/pnpm store,
# the test tree, or any compiler/linter cache. Matched as a path segment
# (a directory name anywhere in the relative path) for the directory
# entries, and by basename or glob for the file entries.
_EXCLUDE_DIR_SEGMENTS = frozenset(
    {
        "dist",
        "node_modules",
        ".venv",
        ".pnpm-store",
        "tests",
        "__pycache__",
        ".pytest_cache",
        ".mypy_cache",
        ".ruff_cache",
        "target",
    }
)
# Two-segment directory paths to exclude (the GCS source and its deps live
# under gcs/, but the built bundle ``gcs/plugin.bundle.js`` must stay).
_EXCLUDE_DIR_PREFIXES = (
    "gcs/src/",
    "gcs/node_modules/",
)
# Exact tooling-config basenames that have no place in a shipped archive.
_EXCLUDE_FILENAMES = frozenset(
    {
        "package.json",
        "tsconfig.json",
        "vitest.config.ts",
    }
)
# Basename glob patterns: build configs and compiler caches by extension.
_EXCLUDE_FILENAME_GLOBS = (
    "esbuild.config.*",
    "*.tsbuildinfo",
)


def _is_excluded_from_pack(rel: str) -> bool:
    """True when ``rel`` (a posix relative path) is build/source/cache cruft
    that must not be packed into a shipped ``.adosplug`` archive."""
    posix = rel
    parts = posix.split("/")
    # Any matching directory segment anywhere in the path is excluded
    # (covers e.g. ``gcs/dist/...``, ``agent/foo/__pycache__/...``).
    if any(seg in _EXCLUDE_DIR_SEGMENTS for seg in parts[:-1]):
        return True
    # ``*.egg-info`` directory segments (name carries the package name).
    if any(seg.endswith(".egg-info") for seg in parts[:-1]):
        return True
    if any(posix.startswith(prefix) for prefix in _EXCLUDE_DIR_PREFIXES):
        return True
    name = parts[-1]
    if name in _EXCLUDE_FILENAMES:
        return True
    if any(fnmatch.fnmatch(name, pat) for pat in _EXCLUDE_FILENAME_GLOBS):
        return True
    return False


@dataclass(frozen=True)
class ArchiveContents:
    manifest: PluginManifest
    payload_hash: bytes
    signer_id: str | None
    signature_b64: str | None
    raw_archive_bytes: bytes


SYMLINK_MODE = 0o120000


def _safe_member_path(name: str) -> str:
    if name.startswith("/") or "\\" in name:
        raise ArchiveError(f"unsafe archive entry path: {name!r}")
    parts = Path(name).parts
    if ".." in parts or any(p.startswith("..") for p in parts):
        raise ArchiveError(f"unsafe archive entry path: {name!r}")
    return name


def _is_symlink_entry(info: zipfile.ZipInfo) -> bool:
    """Detect symlink entries via the upper 16 bits of external_attr.

    Unix file modes ride in ``external_attr >> 16``. Symlinks have the
    ``0o120000`` mode bits set. Reject these — when unpacked, a symlink
    can target arbitrary paths outside the install dir even if the
    entry name itself is innocent.
    """
    mode = (info.external_attr >> 16) & 0xFFFF
    return (mode & 0o170000) == SYMLINK_MODE


def _canonical_payload_hash(entries: dict[str, bytes]) -> bytes:
    """Compute the deterministic payload hash over manifest + assets.

    Sort by path. Concatenate ``"<path>\\n<hex sha256>\\n"`` for each
    entry. Hash the concatenation. Excludes :data:`SIGNATURE_FILENAME`.
    """
    h = hashlib.sha256()
    for path in sorted(entries.keys()):
        if path == SIGNATURE_FILENAME:
            continue
        digest = hashlib.sha256(entries[path]).hexdigest()
        h.update(f"{path}\n{digest}\n".encode())
    return h.digest()


def open_archive(path: str | Path) -> ArchiveContents:
    """Open and parse a ``.adosplug`` archive without verifying signature.

    Validates structural sanity (zip well-formed, manifest present and
    parseable, no path traversal, size bounds). Signature verification
    is a separate step in :mod:`ados.plugins.signing` so callers can
    surface different failures with different exit codes.
    """
    p = Path(path)
    if not p.exists():
        raise ArchiveError(f"archive not found: {path}")
    raw = p.read_bytes()
    if len(raw) > ARCHIVE_MAX_BYTES:
        raise ArchiveError(
            f"archive {path} is {len(raw)} bytes; cap is {ARCHIVE_MAX_BYTES}"
        )

    return parse_archive_bytes(raw)


def parse_archive_bytes(raw: bytes) -> ArchiveContents:
    """Parse archive bytes already in memory."""
    if len(raw) > ARCHIVE_MAX_BYTES:
        raise ArchiveError(
            f"archive is {len(raw)} bytes; cap is {ARCHIVE_MAX_BYTES}"
        )

    try:
        zf = zipfile.ZipFile(io.BytesIO(raw))
    except zipfile.BadZipFile as exc:
        raise ArchiveError(f"not a valid zip archive: {exc}") from exc

    entries: dict[str, bytes] = {}
    try:
        for info in zf.infolist():
            name = _safe_member_path(info.filename)
            if name.endswith("/"):
                continue
            if _is_symlink_entry(info):
                raise ArchiveError(
                    f"archive entry {name} is a symlink; symlinks not allowed"
                )
            if info.file_size > ENTRY_MAX_BYTES:
                raise ArchiveError(
                    f"archive entry {name} is {info.file_size} bytes; "
                    f"per-entry cap is {ENTRY_MAX_BYTES}"
                )
            entries[name] = zf.read(info.filename)
    finally:
        zf.close()

    manifest_bytes = entries.get(MANIFEST_FILENAME)
    if manifest_bytes is None:
        raise ArchiveError(f"archive missing {MANIFEST_FILENAME}")

    try:
        manifest = PluginManifest.from_yaml_text(manifest_bytes.decode("utf-8"))
    except ManifestError:
        raise
    except UnicodeDecodeError as exc:
        raise ArchiveError(f"manifest is not valid UTF-8: {exc}") from exc

    payload_hash = _canonical_payload_hash(entries)
    signer_id, signature_b64 = _read_signature(entries.get(SIGNATURE_FILENAME))

    return ArchiveContents(
        manifest=manifest,
        payload_hash=payload_hash,
        signer_id=signer_id,
        signature_b64=signature_b64,
        raw_archive_bytes=raw,
    )


def _read_signature(blob: bytes | None) -> tuple[str | None, str | None]:
    if blob is None:
        return None, None
    try:
        text = blob.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise SignatureError(
            SignatureError.KIND_INVALID,
            f"SIGNATURE is not valid UTF-8: {exc}",
        ) from exc
    lines = [line.strip() for line in text.splitlines() if line.strip()]
    if len(lines) != 2:
        raise SignatureError(
            SignatureError.KIND_INVALID,
            f"SIGNATURE must be 2 non-blank lines (signer-id + sig), got {len(lines)}",
        )
    return lines[0], lines[1]


def _restore_exec_mode(target: Path, info: zipfile.ZipInfo) -> None:
    """Restore the executable Unix mode when the zip entry carried one.

    An unpacked ``agent/bin/<id>`` Rust-plugin binary must be runnable by the
    generated systemd ``ExecStart``; ``write_bytes`` otherwise leaves the file
    at the umask default (typically ``0644``) and the unit dies with
    ``EACCES``. Only acts when an exec bit is set, and preserves the entry's
    own permission bits (masked to ``0o777``). The canonical payload hash is
    over file content, so restoring the mode never affects the signature.
    """
    mode = (info.external_attr >> 16) & 0o777
    if mode & 0o111:
        os.chmod(target, mode)


def unpack_to(archive_bytes: bytes, dest: Path) -> None:
    """Unpack archive bytes to ``dest`` directory. Caller is responsible for
    having validated the archive (signature etc.) first."""
    dest.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(io.BytesIO(archive_bytes)) as zf:
        for info in zf.infolist():
            name = _safe_member_path(info.filename)
            if name.endswith("/"):
                continue
            if _is_symlink_entry(info):
                raise ArchiveError(
                    f"archive entry {name} is a symlink; symlinks not allowed"
                )
            target = dest / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(zf.read(info.filename))
            _restore_exec_mode(target, info)


def _required_entrypoints(manifest: PluginManifest) -> list[tuple[str, str]]:
    """The archive-relative files a manifest's declared halves require.

    Returns ``(label, relative_path)`` pairs for every entrypoint that
    must be present as a real file in the packed/unpacked archive:

    * the GCS bundle at ``gcs.entrypoint`` when a ``gcs`` block exists;
    * the agent binary at ``agent.entrypoint`` when ``agent.runtime`` is
      ``rust`` (a Python agent's ``module:Class`` entrypoint is resolved
      by the runner, not a packed file, so it is skipped).

    A ``module:Class`` value (it contains a ``:``) is never a packed
    file and is excluded from the must-exist set.
    """
    required: list[tuple[str, str]] = []
    if manifest.gcs is not None and ":" not in manifest.gcs.entrypoint:
        required.append(("gcs.entrypoint", manifest.gcs.entrypoint))
    if (
        manifest.agent is not None
        and manifest.agent.runtime == "rust"
        and ":" not in manifest.agent.entrypoint
    ):
        required.append(("agent.entrypoint", manifest.agent.entrypoint))
    return required


def verify_entrypoints_present(
    manifest: PluginManifest, present_paths: set[str]
) -> None:
    """Assert every must-exist entrypoint is in ``present_paths``.

    ``present_paths`` is the set of archive-relative posix paths actually
    in the archive (packed or unpacked). Raises :class:`ArchiveError`
    naming the missing entrypoint so a half-archive fails loudly instead
    of dying later at iframe-load or unit-start.
    """
    for label, rel in _required_entrypoints(manifest):
        if rel not in present_paths:
            raise ArchiveError(
                f"plugin {manifest.id}: manifest declares {label} {rel!r} "
                f"but that file is not present in the archive"
            )


def pack_directory(src: Path, manifest: PluginManifest, output: Path) -> Path:
    """Build an unsigned ``.adosplug`` archive from a source directory.

    Used by ``ados-plugin pack`` in the dev tool. The signing step is
    separate (``ados-plugin sign``).

    Build, source-tree, test, and cache cruft (see
    :func:`_is_excluded_from_pack`) is skipped so a shipped archive
    carries only the built GCS bundle, the Python agent half, the
    manifest, and runtime assets. After packing, every entrypoint a
    declared half requires must be present, or the build fails loudly.
    """
    output.parent.mkdir(parents=True, exist_ok=True)
    packed: set[str] = {MANIFEST_FILENAME}
    with zipfile.ZipFile(output, "w", zipfile.ZIP_DEFLATED) as zf:
        # Always write the manifest first, deterministically.
        zf.writestr(MANIFEST_FILENAME, _serialize_manifest(manifest))
        for path in sorted(src.rglob("*")):
            if not path.is_file():
                continue
            rel = path.relative_to(src).as_posix()
            if rel == MANIFEST_FILENAME or rel == SIGNATURE_FILENAME:
                continue
            if _is_excluded_from_pack(rel):
                continue
            if path.stat().st_size > ENTRY_MAX_BYTES:
                raise ArchiveError(
                    f"asset {rel} exceeds per-entry cap {ENTRY_MAX_BYTES}"
                )
            zf.write(path, rel)
            packed.add(rel)

    # A half-archive (a declared GCS bundle or rust binary that never made
    # it in) must fail at pack time, not silently ship.
    try:
        verify_entrypoints_present(manifest, packed)
    except ArchiveError:
        output.unlink(missing_ok=True)
        raise
    return output


def _serialize_manifest(manifest: PluginManifest) -> bytes:
    import yaml

    payload = manifest.model_dump(mode="json", exclude_none=True)
    return yaml.safe_dump(payload, sort_keys=True).encode("utf-8")
