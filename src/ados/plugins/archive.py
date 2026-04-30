"""``.adosplug`` archive packer and unpacker.

Archive layout (zip, no compression for binary stability):

    manifest.yaml                   required
    SIGNATURE                       optional, format below
    agent/                          optional, agent half
        wheel/<plugin>-<ver>-py3-none-any.whl
        py/                         OR loose Python source for inprocess plugins
    gcs/                            optional, GCS half
        dist/index.js
        dist/style.css
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

import hashlib
import io
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
        h.update(f"{path}\n{digest}\n".encode("utf-8"))
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


def pack_directory(src: Path, manifest: PluginManifest, output: Path) -> Path:
    """Build an unsigned ``.adosplug`` archive from a source directory.

    Used by ``ados-plugin pack`` in the dev tool. The signing step is
    separate (``ados-plugin sign``).
    """
    output.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(output, "w", zipfile.ZIP_DEFLATED) as zf:
        # Always write the manifest first, deterministically.
        zf.writestr(MANIFEST_FILENAME, _serialize_manifest(manifest))
        for path in sorted(src.rglob("*")):
            if not path.is_file():
                continue
            rel = path.relative_to(src).as_posix()
            if rel == MANIFEST_FILENAME or rel == SIGNATURE_FILENAME:
                continue
            if path.stat().st_size > ENTRY_MAX_BYTES:
                raise ArchiveError(
                    f"asset {rel} exceeds per-entry cap {ENTRY_MAX_BYTES}"
                )
            zf.write(path, rel)
    return output


def _serialize_manifest(manifest: PluginManifest) -> bytes:
    import yaml

    payload = manifest.model_dump(mode="json", exclude_none=True)
    return yaml.safe_dump(payload, sort_keys=True).encode("utf-8")
