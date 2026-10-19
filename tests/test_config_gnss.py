"""The ``gnss`` config block's receiver ``source`` grammar.

The ``udp:`` vectors are shared with the ``ados-gnss`` reader
(``crates/ados-gnss/src/config.rs``), so a source the config surface accepts is
one the service runs, and the other way round.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from pydantic import ValidationError

from ados.core.config import GnssReceiverConfig

_VECTORS = json.loads(
    (Path(__file__).parent / "fixtures" / "gnss-udp-sources.json").read_text("utf-8")
)


@pytest.mark.parametrize("source", _VECTORS["valid"])
def test_valid_udp_sources_are_accepted(source: str) -> None:
    assert GnssReceiverConfig(id="gnss0", source=source).source == source


@pytest.mark.parametrize("source", _VECTORS["invalid"])
def test_invalid_udp_sources_are_refused(source: str) -> None:
    with pytest.raises(ValidationError):
        GnssReceiverConfig(id="gnss0", source=source)


@pytest.mark.parametrize("source", ["hub", "serial:/dev/ttyUSB0", "serial:/dev/ttyACM0@115200"])
def test_other_sources_are_accepted(source: str) -> None:
    assert GnssReceiverConfig(id="gnss0", source=source).source == source
