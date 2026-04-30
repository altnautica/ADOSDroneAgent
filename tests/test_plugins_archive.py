"""Plugin .adosplug archive packer/unpacker tests."""

from __future__ import annotations

import io
import zipfile
from pathlib import Path

import pytest

from ados.plugins.archive import (
    ARCHIVE_MAX_BYTES,
    MANIFEST_FILENAME,
    SIGNATURE_FILENAME,
    open_archive,
    parse_archive_bytes,
    pack_directory,
    unpack_to,
)
from ados.plugins.errors import ArchiveError, SignatureError
from ados.plugins.manifest import PluginManifest


def _good_manifest_yaml() -> str:
    return """\
schema_version: 1
id: com.example.basic
version: 0.1.0
name: Basic
license: GPL-3.0-or-later
risk: low
compatibility:
  ados_version: ">=0.9.0,<1.0.0"
agent:
  entrypoint: agent/plugin.py
  isolation: subprocess
  permissions:
    - event.publish
"""


def _make_zip(entries: dict[str, bytes]) -> bytes:
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as zf:
        for name, data in entries.items():
            zf.writestr(name, data)
    return buf.getvalue()


def test_open_archive_round_trip(tmp_path: Path) -> None:
    archive_bytes = _make_zip(
        {
            MANIFEST_FILENAME: _good_manifest_yaml().encode(),
            "agent/plugin.py": b"# stub\n",
        }
    )
    archive_path = tmp_path / "basic.adosplug"
    archive_path.write_bytes(archive_bytes)

    contents = open_archive(archive_path)
    assert contents.manifest.id == "com.example.basic"
    assert contents.signer_id is None
    assert contents.signature_b64 is None
    assert isinstance(contents.payload_hash, bytes) and len(contents.payload_hash) == 32


def test_archive_missing_manifest_raises() -> None:
    archive_bytes = _make_zip({"random.txt": b"hi"})
    with pytest.raises(ArchiveError):
        parse_archive_bytes(archive_bytes)


def test_archive_traversal_rejected() -> None:
    archive_bytes = _make_zip(
        {
            MANIFEST_FILENAME: _good_manifest_yaml().encode(),
            "../escape.py": b"oops",
        }
    )
    with pytest.raises(ArchiveError):
        parse_archive_bytes(archive_bytes)


def test_archive_size_cap_enforced() -> None:
    too_big = b"x" * (ARCHIVE_MAX_BYTES + 1)
    with pytest.raises(ArchiveError):
        parse_archive_bytes(too_big)


def test_signature_well_formed_round_trips() -> None:
    sig_blob = b"altnautica-2026-A\nQUJDREVGRw==\n"
    archive_bytes = _make_zip(
        {
            MANIFEST_FILENAME: _good_manifest_yaml().encode(),
            SIGNATURE_FILENAME: sig_blob,
        }
    )
    contents = parse_archive_bytes(archive_bytes)
    assert contents.signer_id == "altnautica-2026-A"
    assert contents.signature_b64 == "QUJDREVGRw=="


def test_signature_malformed_raises() -> None:
    sig_blob = b"only-one-line\n"
    archive_bytes = _make_zip(
        {
            MANIFEST_FILENAME: _good_manifest_yaml().encode(),
            SIGNATURE_FILENAME: sig_blob,
        }
    )
    with pytest.raises(SignatureError):
        parse_archive_bytes(archive_bytes)


def test_pack_and_unpack_round_trip(tmp_path: Path) -> None:
    src = tmp_path / "src"
    src.mkdir()
    (src / "agent").mkdir()
    (src / "agent" / "plugin.py").write_text("# stub")

    manifest = PluginManifest.from_yaml_text(_good_manifest_yaml())
    out = tmp_path / "basic.adosplug"
    pack_directory(src, manifest, out)
    assert out.exists()

    raw = out.read_bytes()
    contents = parse_archive_bytes(raw)
    assert contents.manifest.id == "com.example.basic"

    dest = tmp_path / "unpacked"
    unpack_to(raw, dest)
    assert (dest / MANIFEST_FILENAME).exists()
    assert (dest / "agent" / "plugin.py").read_text() == "# stub"


def test_payload_hash_deterministic() -> None:
    archive_a = _make_zip(
        {
            MANIFEST_FILENAME: _good_manifest_yaml().encode(),
            "agent/plugin.py": b"# stub",
        }
    )
    archive_b = _make_zip(
        {
            "agent/plugin.py": b"# stub",
            MANIFEST_FILENAME: _good_manifest_yaml().encode(),
        }
    )
    a = parse_archive_bytes(archive_a)
    b = parse_archive_bytes(archive_b)
    assert a.payload_hash == b.payload_hash, (
        "payload hash must be order-independent across zip entries"
    )
