"""Manifest loaders for the Peripheral Manager.

Two sources are supported:

1. Python entry points under the ``ados.peripherals`` group. Each
   loaded object is expected to expose either a ``manifest`` class
   attribute of type :class:`PeripheralManifest` or a
   ``get_manifest()`` classmethod that returns one. Plugins shipped
   as pip packages register here.

2. YAML files under a glob, default ``/etc/ados/peripherals/*.yaml``.
   Packagers, integrators, and bench operators drop files here without
   touching Python.

Merge rule: entry-point manifests win on ``id`` collision. The
rationale is that pip-installed plugins represent the supported code
path, while YAML files are local overrides that exist to extend, not
replace, plugin behavior.
"""

from __future__ import annotations

import glob
from importlib import metadata

from ados.core.logging import get_logger
from ados.core.paths import PERIPHERALS_GLOB
from ados.services.peripherals.manifest import ManifestError, PeripheralManifest

log = get_logger("peripherals.loader")

_ENTRY_POINT_GROUP = "ados.peripherals"
_DEFAULT_GLOB = PERIPHERALS_GLOB


def _extract_manifest(obj: object, ep_name: str) -> PeripheralManifest | None:
    """Extract a PeripheralManifest from a loaded entry-point object.

    Accepts three shapes:
      - an instance of PeripheralManifest
      - a class or object with a ``manifest`` attribute that is one
      - a class or object with a ``get_manifest()`` callable
    """
    if isinstance(obj, PeripheralManifest):
        return obj

    manifest_attr = getattr(obj, "manifest", None)
    if isinstance(manifest_attr, PeripheralManifest):
        return manifest_attr

    getter = getattr(obj, "get_manifest", None)
    if callable(getter):
        try:
            result = getter()
        except Exception as exc:  # noqa: BLE001
            log.warning(
                "peripheral_entry_point_getter_failed",
                entry_point=ep_name,
                error=str(exc),
            )
            return None
        if isinstance(result, PeripheralManifest):
            return result
        log.warning(
            "peripheral_entry_point_bad_return",
            entry_point=ep_name,
            returned_type=type(result).__name__,
        )
        return None

    log.warning(
        "peripheral_entry_point_no_manifest",
        entry_point=ep_name,
        loaded_type=type(obj).__name__,
    )
    return None


def load_from_entry_points() -> list[PeripheralManifest]:
    """Load manifests from the ``ados.peripherals`` entry-point group.

    Gracefully handles missing group, import errors per entry, and
    malformed plugins. One bad plugin does not kill the rest.
    """
    manifests: list[PeripheralManifest] = []

    try:
        eps = metadata.entry_points()
        # Python 3.10+ select API. The older .get() path is gone as of 3.12.
        group = eps.select(group=_ENTRY_POINT_GROUP) if hasattr(eps, "select") else []
    except Exception as exc:  # noqa: BLE001
        log.warning("peripheral_entry_points_lookup_failed", error=str(exc))
        return manifests

    for ep in group:
        try:
            loaded = ep.load()
        except Exception as exc:  # noqa: BLE001
            log.warning(
                "peripheral_entry_point_load_failed",
                entry_point=ep.name,
                error=str(exc),
            )
            continue

        manifest = _extract_manifest(loaded, ep.name)
        if manifest is not None:
            manifests.append(manifest)
            log.info(
                "peripheral_entry_point_loaded",
                entry_point=ep.name,
                peripheral_id=manifest.id,
            )

    return manifests


def load_from_filesystem(
    glob_path: str = _DEFAULT_GLOB,
) -> list[PeripheralManifest]:
    """Load manifests from YAML files matching ``glob_path``.

    Each file failure is logged but does not abort the scan.
    """
    manifests: list[PeripheralManifest] = []

    for path in sorted(glob.glob(glob_path)):
        try:
            manifest = PeripheralManifest.from_yaml_file(path)
        except ManifestError as exc:
            log.warning(
                "peripheral_yaml_load_failed",
                path=path,
                error=str(exc),
            )
            continue
        manifests.append(manifest)
        log.info(
            "peripheral_yaml_loaded",
            path=path,
            peripheral_id=manifest.id,
        )

    return manifests


def load_all(glob_path: str = _DEFAULT_GLOB) -> list[PeripheralManifest]:
    """Merge entry-point and filesystem manifests.

    Entry-point manifests win on id collision. Order within source is
    preserved; overall order is entry points first, then non-colliding
    filesystem manifests in sorted-path order.
    """
    ep_manifests = load_from_entry_points()
    fs_manifests = load_from_filesystem(glob_path)

    by_id: dict[str, PeripheralManifest] = {m.id: m for m in ep_manifests}
    for fs_manifest in fs_manifests:
        if fs_manifest.id in by_id:
            log.info(
                "peripheral_manifest_collision",
                peripheral_id=fs_manifest.id,
                winner="entry_point",
            )
            continue
        by_id[fs_manifest.id] = fs_manifest

    # Preserve entry-point order, then append filesystem-only.
    merged: list[PeripheralManifest] = list(ep_manifests)
    ep_ids = {m.id for m in ep_manifests}
    for fs_manifest in fs_manifests:
        if fs_manifest.id not in ep_ids:
            merged.append(fs_manifest)

    return merged
