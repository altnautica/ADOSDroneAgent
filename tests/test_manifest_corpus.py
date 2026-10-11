"""The shared plugin-manifest conformance corpus through the schema-source model.

The same fixtures run through the Rust install parser
(``crates/ados-plugin-host/tests/manifest_corpus.rs``), so the two validators
accept and refuse the same manifests.
"""

from __future__ import annotations

import re
from pathlib import Path

import pytest

from ados.plugins.errors import ManifestError
from ados.plugins.manifest import PluginManifest

CORPUS = Path(__file__).resolve().parents[1] / "schemas" / "fixtures" / "plugin-manifest"


def _fixtures(kind: str) -> list[Path]:
    return sorted((CORPUS / kind).glob("*.yaml"))


@pytest.mark.parametrize("path", _fixtures("valid"), ids=lambda p: p.stem)
def test_valid_fixture_parses(path: Path) -> None:
    PluginManifest.from_yaml_text(path.read_text(encoding="utf-8"))


@pytest.mark.parametrize("path", _fixtures("invalid"), ids=lambda p: p.stem)
def test_invalid_fixture_is_refused(path: Path) -> None:
    with pytest.raises(ManifestError):
        PluginManifest.from_yaml_text(path.read_text(encoding="utf-8"))


def test_every_invalid_fixture_names_its_rule() -> None:
    expected = (CORPUS / "invalid" / "expected.toml").read_text(encoding="utf-8")
    listed = set(re.findall(r"^\[([^\]]+)\]$", expected, flags=re.MULTILINE))
    present = {p.stem for p in _fixtures("invalid")}
    assert listed == present


def test_corpus_is_not_empty() -> None:
    assert _fixtures("valid")
    assert _fixtures("invalid")
