"""Every config the model refuses, the native config write refuses too.

``tests/fixtures/config-invalid/`` holds one document per refusal the config
model makes (an enum, a bound, a cross-field ``model_validator``). Each one
must fail model validation here, and each one's file name must be listed in the
native write's own parity test, which walks the same directory and asserts the
native validator refuses the document. A model rule added without a native
mirror therefore fails one of the two suites.
"""

from __future__ import annotations

from pathlib import Path

import pytest
import yaml
from pydantic import ValidationError

import ados.core.identity as identity
from ados.core.config import ADOSConfig
from ados.core.config.writer import baseline_model

_ROOT = Path(__file__).resolve().parents[1]
_FIXTURES = _ROOT / "tests" / "fixtures" / "config-invalid"
_NATIVE_TESTS = _ROOT / "crates" / "ados-control" / "src" / "routes" / "config_rw" / "tests.rs"
_FIXTURE_NAMES = sorted(p.name for p in _FIXTURES.glob("*.yaml"))


def test_there_are_fixtures() -> None:
    assert _FIXTURE_NAMES, f"no fixtures under {_FIXTURES}"


@pytest.mark.parametrize("name", _FIXTURE_NAMES)
def test_the_model_refuses_each_fixture(name: str, monkeypatch) -> None:
    # Identity resolution would read the host's device-id file; pin it.
    monkeypatch.setattr(identity, "resolve_device_id", lambda *_a, **_k: "fixture-node")
    document = yaml.safe_load((_FIXTURES / name).read_text(encoding="utf-8"))
    with pytest.raises(ValidationError):
        baseline_model(ADOSConfig, document)


@pytest.mark.parametrize("name", _FIXTURE_NAMES)
def test_the_native_parity_list_names_each_fixture(name: str) -> None:
    native = _NATIVE_TESTS.read_text(encoding="utf-8")
    assert f'"{name}"' in native, (
        f"{name} is refused by the model but not listed in the native parity test "
        f"({_NATIVE_TESTS.relative_to(_ROOT)})"
    )
