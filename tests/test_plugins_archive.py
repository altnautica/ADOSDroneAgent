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
    pack_directory,
    parse_archive_bytes,
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


def test_unpack_restores_exec_bit_for_executable_entries(tmp_path: Path) -> None:
    """An exec-marked agent/bin entry must come back runnable (systemd
    ExecStart needs it); a plain entry must stay non-executable."""
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as zf:
        zf.writestr(MANIFEST_FILENAME, _good_manifest_yaml().encode())
        # An executable binary entry: external_attr upper 16 bits carry 0o755.
        info = zipfile.ZipInfo("agent/bin/geofence")
        info.external_attr = 0o755 << 16
        zf.writestr(info, b"#!/bin/sh\n")
    raw = buf.getvalue()

    dest = tmp_path / "unpacked"
    unpack_to(raw, dest)

    bin_mode = (dest / "agent" / "bin" / "geofence").stat().st_mode
    assert bin_mode & 0o111, f"agent/bin entry must be executable (mode {oct(bin_mode)})"
    mani_mode = (dest / MANIFEST_FILENAME).stat().st_mode
    assert not (mani_mode & 0o111), "plain entry must not gain exec bits"


def _manifest_with_gcs_yaml(gcs_entrypoint: str = "gcs/plugin.bundle.js") -> str:
    return f"""\
schema_version: 1
id: com.example.gcs
version: 0.1.0
name: Gcs
license: GPL-3.0-or-later
risk: low
compatibility:
  ados_version: ">=0.9.0,<1.0.0"
agent:
  entrypoint: agent/plugin.py
  isolation: subprocess
  permissions:
    - event.publish
gcs:
  entrypoint: {gcs_entrypoint}
  isolation: iframe
  permissions:
    - ui.slot.fc-tab
"""


def test_pack_excludes_source_and_cache_cruft(tmp_path: Path) -> None:
    """A tree carrying GCS source, deps, tests, and caches must pack only
    the runtime files — never the cruft a shipped archive should omit."""
    src = tmp_path / "src"
    (src / "agent").mkdir(parents=True)
    (src / "agent" / "plugin.py").write_text("# stub")
    (src / "agent" / "__pycache__").mkdir()
    (src / "agent" / "__pycache__" / "plugin.cpython-313.pyc").write_bytes(b"\x00")
    # Built GCS bundle stays; gcs/src and gcs/node_modules drop.
    (src / "gcs").mkdir()
    (src / "gcs" / "plugin.bundle.js").write_text("export const x = 1;")
    (src / "gcs" / "src").mkdir()
    (src / "gcs" / "src" / "index.ts").write_text("// source")
    (src / "gcs" / "node_modules").mkdir()
    (src / "gcs" / "node_modules" / "dep.js").write_text("// dep")
    (src / "gcs" / "dist").mkdir()
    (src / "gcs" / "dist" / "leftover.js").write_text("// stale build output")
    (src / "node_modules").mkdir()
    (src / "node_modules" / "root-dep.js").write_text("// root dep")
    (src / "tests").mkdir()
    (src / "tests" / "test_plugin.py").write_text("# test")
    (src / ".ruff_cache").mkdir()
    (src / ".ruff_cache" / "cache").write_bytes(b"\x00")
    (src / "package.json").write_text("{}")
    (src / "tsconfig.json").write_text("{}")
    (src / "vitest.config.ts").write_text("// config")
    (src / "esbuild.config.mjs").write_text("// config")
    (src / "buildinfo.tsbuildinfo").write_text("{}")
    egg = src / "com_example_gcs.egg-info"
    egg.mkdir()
    (egg / "PKG-INFO").write_text("Metadata")
    # Runtime assets that must survive.
    (src / "icon.png").write_bytes(b"\x89PNG\r\n")
    (src / "README.md").write_text("readme")
    (src / "config-schema.json").write_text("{}")
    (src / "locales").mkdir()
    (src / "locales" / "en.json").write_text("{}")

    manifest = PluginManifest.from_yaml_text(_manifest_with_gcs_yaml())
    out = tmp_path / "gcs.adosplug"
    pack_directory(src, manifest, out)

    raw = out.read_bytes()
    names = set(zipfile.ZipFile(io.BytesIO(raw)).namelist())

    # Cruft excluded.
    assert not any(n.startswith("gcs/src/") for n in names)
    assert not any(n.startswith("gcs/node_modules/") for n in names)
    assert not any(n.startswith("gcs/dist/") for n in names)
    assert not any(n.startswith("node_modules/") for n in names)
    assert not any(n.startswith("tests/") for n in names)
    assert not any("__pycache__" in n for n in names)
    assert not any(".ruff_cache" in n for n in names)
    assert not any(".egg-info" in n for n in names)
    assert "package.json" not in names
    assert "tsconfig.json" not in names
    assert "vitest.config.ts" not in names
    assert "esbuild.config.mjs" not in names
    assert "buildinfo.tsbuildinfo" not in names

    # Runtime files kept.
    assert MANIFEST_FILENAME in names
    assert "agent/plugin.py" in names
    assert "gcs/plugin.bundle.js" in names
    assert "icon.png" in names
    assert "README.md" in names
    assert "config-schema.json" in names
    assert "locales/en.json" in names

    # And the packed archive round-trips back through the parser.
    contents = parse_archive_bytes(raw)
    assert contents.manifest.id == "com.example.gcs"


def test_pack_raises_when_declared_gcs_bundle_missing(tmp_path: Path) -> None:
    """A manifest declaring a GCS entrypoint with no built bundle present
    must fail loudly at pack time, and not leave a half-archive behind."""
    src = tmp_path / "src"
    (src / "agent").mkdir(parents=True)
    (src / "agent" / "plugin.py").write_text("# stub")
    # No gcs/plugin.bundle.js on disk.

    manifest = PluginManifest.from_yaml_text(_manifest_with_gcs_yaml())
    out = tmp_path / "broken.adosplug"
    with pytest.raises(ArchiveError) as exc:
        pack_directory(src, manifest, out)
    assert "gcs.entrypoint" in str(exc.value)
    assert "gcs/plugin.bundle.js" in str(exc.value)
    assert not out.exists()


def test_pack_complete_gcs_tree_round_trips(tmp_path: Path) -> None:
    src = tmp_path / "src"
    (src / "agent").mkdir(parents=True)
    (src / "agent" / "plugin.py").write_text("# stub")
    (src / "gcs").mkdir()
    (src / "gcs" / "plugin.bundle.js").write_text("export const x = 1;")

    manifest = PluginManifest.from_yaml_text(_manifest_with_gcs_yaml())
    out = tmp_path / "ok.adosplug"
    pack_directory(src, manifest, out)
    assert out.exists()

    raw = out.read_bytes()
    dest = tmp_path / "unpacked"
    unpack_to(raw, dest)
    assert (dest / "gcs" / "plugin.bundle.js").read_text() == "export const x = 1;"
    assert (dest / "agent" / "plugin.py").read_text() == "# stub"


def test_pack_raises_when_rust_binary_missing(tmp_path: Path) -> None:
    """A rust-runtime agent half whose entrypoint binary is absent must
    fail at pack time the same way a missing GCS bundle does."""
    src = tmp_path / "src"
    src.mkdir()
    manifest_yaml = """\
schema_version: 1
id: com.example.rust
version: 0.1.0
name: Rust
license: GPL-3.0-or-later
risk: low
compatibility:
  ados_version: ">=0.9.0,<1.0.0"
agent:
  entrypoint: agent/bin/com.example.rust
  isolation: subprocess
  runtime: rust
  permissions:
    - event.publish
"""
    manifest = PluginManifest.from_yaml_text(manifest_yaml)
    out = tmp_path / "rust.adosplug"
    with pytest.raises(ArchiveError) as exc:
        pack_directory(src, manifest, out)
    assert "agent.entrypoint" in str(exc.value)
    assert not out.exists()


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
