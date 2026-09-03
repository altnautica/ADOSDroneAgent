"""YAML read/write helpers shared by the config loader and the maintenance pass.

The one thing here that is not boilerplate is
:class:`StringTimestampLoader`. The config writers persist timestamps
(``video.wfb.paired_at`` and friends) as unquoted ISO-8601 values. The stock
loader resolves those to ``datetime``, which then fails the str-typed config
fields — and, worse for a read-modify-write pass, ``safe_dump`` of the
resulting ``datetime`` writes back a *different* string than the one that was
read, silently rewriting a pairing timestamp on any unrelated migration.
Dropping the timestamp implicit resolver keeps every unquoted timestamp a
string on the read side, so the YAML written by any process round-trips
unchanged.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

import yaml


class StringTimestampLoader(yaml.SafeLoader):
    """A SafeLoader that keeps ISO-8601 timestamps as plain strings."""


StringTimestampLoader.yaml_implicit_resolvers = {
    first_char: [
        (tag, regexp)
        for tag, regexp in resolvers
        if tag != "tag:yaml.org,2002:timestamp"
    ]
    for first_char, resolvers in yaml.SafeLoader.yaml_implicit_resolvers.items()
}


def load_mapping(text: str) -> dict[str, Any]:
    """Parse YAML text into a mapping. Raises ``yaml.YAMLError`` on garbage."""
    loaded = yaml.load(text, Loader=StringTimestampLoader)
    return loaded if isinstance(loaded, dict) else {}


def read_mapping(path: Path) -> dict[str, Any]:
    """Read and parse a YAML file into a mapping."""
    return load_mapping(path.read_text(encoding="utf-8"))


def dump_mapping(data: dict[str, Any]) -> str:
    """Serialize a mapping the way every config writer in the tree does."""
    return yaml.safe_dump(data, sort_keys=False, default_flow_style=False)
