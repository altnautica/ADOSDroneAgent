"""The packaged ``defaults.yaml``, read once per process.

Both the loader (:func:`ados.core.config.load_config`) and the writer's
baseline (:func:`ados.core.config.writer.baseline_model`) merge the on-disk
document over these defaults. They have to use the same bytes: a baseline built
over a *different* default set would report every field the two sets disagree
on as a caller change and write it into the node's file, which is the
default-freezing defect the merge writer exists to remove.
"""

from __future__ import annotations

from typing import Any

import yaml

from ._yaml import StringTimestampLoader

_CACHE: dict[str, Any] | None = None


def packaged_defaults() -> dict[str, Any]:
    """The shipped default mapping. Returns a fresh copy; callers mutate."""
    global _CACHE
    if _CACHE is None:
        _CACHE = _read()
    # A deep copy would be wasted work: every consumer feeds this to
    # ``_deep_merge``, which copies each level it touches and never mutates the
    # base. A shallow copy is enough to stop a caller rebinding a top-level key
    # in the cache.
    return dict(_CACHE)


def _read() -> dict[str, Any]:
    import importlib.resources

    try:
        ref = importlib.resources.files("ados.core").joinpath("defaults.yaml")
        loaded = yaml.load(ref.read_text(encoding="utf-8"), Loader=StringTimestampLoader)
    except (FileNotFoundError, TypeError, OSError, yaml.YAMLError):
        return {}
    return loaded if isinstance(loaded, dict) else {}


__all__ = ["packaged_defaults"]
