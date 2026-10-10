"""Plugin trust: secret-free packing, entry-name hygiene and the revocation list."""

from __future__ import annotations

import io
import json
import zipfile
from pathlib import Path

import pytest

from ados.plugins.archive import pack_directory, parse_archive_bytes
from ados.plugins.errors import ArchiveError
from ados.plugins.manifest import PluginManifest
from ados.plugins.signing import (
    RevocationListError,
    Revocations,
    load_revocations,
    merge_revocations,
    parse_revocations,
)

MANIFEST = """\
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


def _zip(entries: dict[str, bytes]) -> bytes:
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_STORED) as zf:
        for name, data in entries.items():
            zf.writestr(name, data)
    return buf.getvalue()


def test_archive_excludes_secrets(tmp_path: Path) -> None:
    src = tmp_path / "src"
    (src / "agent").mkdir(parents=True)
    (src / "agent" / "plugin.py").write_text("# stub")
    (src / "signing-key.pem").write_text("-----BEGIN PRIVATE KEY-----")
    (src / "agent" / "device.key").write_text("secret")
    (src / ".env").write_text("TOKEN=x")
    (src / ".env.local").write_text("TOKEN=y")
    (src / ".DS_Store").write_bytes(b"\0")
    (src / ".git").mkdir()
    (src / ".git" / "config").write_text("[core]")

    out = tmp_path / "basic.adosplug"
    pack_directory(src, PluginManifest.from_yaml_text(MANIFEST), out)
    with zipfile.ZipFile(out) as zf:
        names = set(zf.namelist())
    assert names == {"manifest.yaml", "agent/plugin.py"}


@pytest.mark.parametrize("bad", ["agent/a.py\nabc\nagent/b.py", "agent/x\r.py", "agent/x\x7f.py"])
def test_safe_member_rejects_newline(bad: str) -> None:
    raw = _zip({"manifest.yaml": MANIFEST.encode(), bad: b"x"})
    with pytest.raises(ArchiveError, match="unsafe archive entry path"):
        parse_archive_bytes(raw)


def test_revocations_v2_parse_and_merge(tmp_path: Path) -> None:
    bare = parse_revocations(b'["alice"]')
    assert bare.signers == {"alice"} and bare.archives == set()
    v2 = parse_revocations('{"signers": ["bob"], "archives": ["ABCD"]}')
    assert v2.signers == {"bob"} and v2.archive_revoked("abcd")
    for garbage in (b'["alice"', b'"alice"', b'{"signers": "alice"}', b"[1]"):
        with pytest.raises(RevocationListError):
            parse_revocations(garbage)

    path = tmp_path / "plugin-revocations.json"
    # A missing file is an empty list.
    assert load_revocations(path) == Revocations()

    path.write_text('["alice"]')
    assert merge_revocations(path, Revocations(signers={"bob"}, archives={"EF01"}))
    merged = load_revocations(path)
    assert merged.signers == {"alice", "bob"}
    assert merged.archives == {"ef01"}
    assert json.loads(path.read_text()) == {"signers": ["alice", "bob"], "archives": ["ef01"]}
    # Nothing new, or an empty incoming list, writes nothing and removes nothing.
    assert not merge_revocations(path, Revocations(signers={"bob"}))
    assert not merge_revocations(path, Revocations())
    assert load_revocations(path) == merged
    # A malformed local file is refused and left untouched.
    path.write_text("{garbled")
    with pytest.raises(RevocationListError):
        merge_revocations(path, Revocations(signers={"carol"}))
    assert path.read_text() == "{garbled"


def test_the_registry_revoked_json_shape_parses_and_merges(tmp_path: Path) -> None:
    # The exact body the registry's `/v1/revoked.json` serves.
    registry = parse_revocations(
        b'{"signer_keys": ["remote-signer"], '
        b'"plugin_versions": ["com.example.thermal@1.2.0"], "generated_at": 1760000000000}'
    )
    assert registry.signers == {"remote-signer"} and registry.archives == set()
    assert registry.version_revoked("com.example.thermal", "1.2.0")
    assert not registry.version_revoked("com.example.thermal", "1.2.1")
    with pytest.raises(RevocationListError):
        parse_revocations(b'{"plugin_versions": ["com.example.thermal"]}')

    path = tmp_path / "plugin-revocations.json"
    path.write_text('{"signers": ["local-signer"], "archives": []}')
    assert merge_revocations(path, registry)
    assert json.loads(path.read_text()) == {
        "signers": ["local-signer", "remote-signer"],
        "archives": [],
        "versions": ["com.example.thermal@1.2.0"],
    }
    assert not merge_revocations(path, registry)


def test_the_shipped_revocation_list_is_empty_and_well_formed() -> None:
    shipped = Path(__file__).resolve().parents[1] / "scripts" / "plugin-keys" / "revoked.json"
    assert load_revocations(shipped) == Revocations()
    assert json.loads(shipped.read_text()) == {"signers": [], "archives": []}
