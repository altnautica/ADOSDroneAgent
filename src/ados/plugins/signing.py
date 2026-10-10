"""Plugin revocation list: parse, load and add-only merge.

The list lives at ``/etc/ados/plugin-revocations.json``. Accepted shapes: a
bare JSON array of revoked signer ids; the local object
``{"signers": [...], "archives": [...], "versions": [...]}`` where
``archives`` holds lowercase sha256 hex digests of revoked ``.adosplug`` files
and ``versions`` holds ``<plugin_id>@<version>`` releases; or the registry's
``/v1/revoked.json`` object ``{"signer_keys": [...], "plugin_versions":
[...], "generated_at": <ms>}``. The plugin host
(``crates/ados-plugin-host/src/signing.rs``) reads the same file the same
way.

A missing file is an empty list. A file that exists but cannot be read or
parsed is an error, never an empty list: a garbled file must not silently
re-trust a revoked signer.
"""

from __future__ import annotations

import json
import os
import tempfile
from dataclasses import dataclass, field
from pathlib import Path

PLUGIN_REVOCATIONS_PATH = Path("/etc/ados/plugin-revocations.json")


class RevocationListError(ValueError):
    """The revocation list exists but is unreadable or malformed."""


@dataclass
class Revocations:
    signers: set[str] = field(default_factory=set)
    archives: set[str] = field(default_factory=set)
    versions: set[str] = field(default_factory=set)

    def archive_revoked(self, sha256_hex: str) -> bool:
        return sha256_hex.lower() in self.archives

    def version_revoked(self, plugin_id: str, version: str) -> bool:
        return f"{plugin_id}@{version}" in self.versions

    def _size(self) -> int:
        return len(self.signers) + len(self.archives) + len(self.versions)

    def to_dict(self) -> dict[str, list[str]]:
        out = {"signers": sorted(self.signers), "archives": sorted(self.archives)}
        if self.versions:
            out["versions"] = sorted(self.versions)
        return out


def _strings(value: object, key: str) -> set[str]:
    if not isinstance(value, list):
        raise RevocationListError(f"revocation list `{key}` is not an array")
    out: set[str] = set()
    for item in value:
        if not isinstance(item, str) or not item.strip():
            raise RevocationListError(f"revocation list `{key}` holds {item!r}")
        out.add(item.strip())
    return out


def parse_revocations(raw: bytes | str) -> Revocations:
    """Parse a revocation document in any accepted shape."""
    try:
        doc = json.loads(raw)
    except (ValueError, UnicodeDecodeError) as exc:
        raise RevocationListError(f"revocation list is not JSON: {exc}") from exc
    if isinstance(doc, list):
        return Revocations(signers=_strings(doc, "signers"))
    if isinstance(doc, dict):
        signers = _strings(doc.get("signers", []), "signers")
        signers |= _strings(doc.get("signer_keys", []), "signer_keys")
        archives = {a.lower() for a in _strings(doc.get("archives", []), "archives")}
        versions = _strings(doc.get("versions", []), "versions")
        versions |= _strings(doc.get("plugin_versions", []), "plugin_versions")
        for entry in versions:
            plugin_id, sep, version = entry.partition("@")
            if not (sep and plugin_id and version):
                raise RevocationListError(
                    f"revocation list version `{entry}` is not <plugin_id>@<version>"
                )
        return Revocations(signers=signers, archives=archives, versions=versions)
    raise RevocationListError("revocation list is neither an array nor an object")


def load_revocations(path: Path = PLUGIN_REVOCATIONS_PATH) -> Revocations:
    """Read the list at ``path``. Missing is empty; malformed raises."""
    try:
        raw = path.read_bytes()
    except FileNotFoundError:
        return Revocations()
    except OSError as exc:
        raise RevocationListError(f"revocation list unreadable: {exc}") from exc
    return parse_revocations(raw)


def merge_revocations(path: Path, incoming: Revocations) -> bool:
    """Merge ``incoming`` into the list at ``path``, add-only.

    No entry is ever removed. The file is rewritten durably (temp file, fsync,
    rename, directory fsync) and only when something was added. Returns
    whether it was. A malformed local file raises and is left untouched.
    """
    current = load_revocations(path)
    before = current._size()
    current.signers |= incoming.signers
    current.archives |= {a.lower() for a in incoming.archives}
    current.versions |= incoming.versions
    if current._size() == before:
        return False
    data = (json.dumps(current.to_dict(), indent=2) + "\n").encode("utf-8")
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=path.name + ".tmp-")
    try:
        with os.fdopen(fd, "wb") as fh:
            fh.write(data)
            fh.flush()
            os.fsync(fh.fileno())
        if path.exists():
            os.chmod(tmp, path.stat().st_mode & 0o777)
        else:
            os.chmod(tmp, 0o644)
        os.replace(tmp, path)
    except BaseException:
        Path(tmp).unlink(missing_ok=True)
        raise
    if hasattr(os, "O_DIRECTORY"):
        dir_fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(dir_fd)
        finally:
            os.close(dir_fd)
    return True
