"""Ed25519 signature verification and revocation list for plugins.

Reuses the OTA-side primitives in :mod:`ados.services.ota.verifier`.
The signing model:

* Signing payload: SHA-256 of the canonical archive contents minus the
  signature file. See :mod:`ados.plugins.archive` for the canonical
  layout.
* Signature format: base64-encoded raw 64-byte Ed25519 signature.
* Trusted-keys store: PEM public keys under ``/etc/ados/plugin-keys/``,
  filename = ``<signer-id>.pem``. The first-party Altnautica signer
  ships at ``altnautica-2026-A.pem``.
* Revocation list: JSON file at ``/etc/ados/plugin-revocations.json``
  containing a list of signer ids that have been retired. Plugins
  signed with a revoked id refuse to load.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path

from ados.core.logging import get_logger
from ados.core.paths import PLUGIN_KEYS_DIR, PLUGIN_REVOCATIONS_PATH
from ados.plugins.errors import SignatureError
from ados.services.ota.verifier import verify_signature

log = get_logger("plugins.signing")


@dataclass(frozen=True)
class TrustedKey:
    signer_id: str
    pem: bytes


def load_trusted_keys(keys_dir: Path | None = None) -> dict[str, TrustedKey]:
    """Load every PEM public key from the trusted-keys directory.

    Returns a dict keyed by ``signer_id`` (the filename stem). Missing
    directory or empty directory returns an empty dict; the caller
    decides whether that is fatal.
    """
    base = Path(keys_dir) if keys_dir is not None else PLUGIN_KEYS_DIR
    keys: dict[str, TrustedKey] = {}
    if not base.exists():
        log.info("plugin_keys_dir_missing", path=str(base))
        return keys
    for path in sorted(base.glob("*.pem")):
        signer_id = path.stem
        try:
            pem = path.read_bytes()
        except OSError as exc:
            log.warning(
                "plugin_key_read_failed",
                signer_id=signer_id,
                error=str(exc),
            )
            continue
        keys[signer_id] = TrustedKey(signer_id=signer_id, pem=pem)
    return keys


def load_revocation_list(path: Path | None = None) -> set[str]:
    """Read the revocation list. Missing file returns empty set."""
    target = Path(path) if path is not None else PLUGIN_REVOCATIONS_PATH
    if not target.exists():
        return set()
    try:
        raw = json.loads(target.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        log.warning("plugin_revocations_read_failed", error=str(exc))
        return set()
    if not isinstance(raw, list):
        log.warning(
            "plugin_revocations_bad_shape",
            actual_type=type(raw).__name__,
        )
        return set()
    return {str(item) for item in raw}


def verify_archive_signature(
    payload_hash: bytes,
    signature_b64: str,
    signer_id: str,
    *,
    trusted_keys: dict[str, TrustedKey] | None = None,
    revocations: set[str] | None = None,
) -> None:
    """Verify the plugin archive signature.

    Raises :class:`SignatureError` with a structured ``kind`` on every
    failure path. Returns ``None`` on success.

    The payload hash is the SHA-256 of the archive's canonical content
    (manifest plus assets, signature file excluded). The actual signing
    payload passed to Ed25519 is ``payload_hash`` itself; we sign the
    32-byte digest, not the full archive, so that signature verification
    runs in constant time independent of archive size.
    """
    if trusted_keys is None:
        trusted_keys = load_trusted_keys()
    if revocations is None:
        revocations = load_revocation_list()

    if signer_id in revocations:
        raise SignatureError(
            SignatureError.KIND_REVOKED,
            f"signer {signer_id} is on the revocation list",
        )

    key = trusted_keys.get(signer_id)
    if key is None:
        raise SignatureError(
            SignatureError.KIND_UNKNOWN_SIGNER,
            f"signer {signer_id} not in /etc/ados/plugin-keys/",
        )

    if not verify_signature(payload_hash, signature_b64, key.pem):
        raise SignatureError(
            SignatureError.KIND_INVALID,
            f"signature does not verify under key {signer_id}",
        )

    log.info("plugin_signature_verified", signer_id=signer_id)


FIRST_PARTY_SIGNERS: frozenset[str] = frozenset(
    {
        "altnautica-2026-A",
        "altnautica-2026-B",
    }
)
"""Hardcoded allowlist of first-party Altnautica signer ids.

Maintained in code rather than via filesystem prefix-matching so a
malicious actor with write access to ``/etc/ados/plugin-keys/`` cannot
plant a key file with the right prefix and impersonate first-party
status. Rotate by adding the new signer id and dropping the retired
one in a deliberate code change. Coordinated with the OTA signing key
rotation in :mod:`ados.services.ota`.
"""


def is_first_party_signer(signer_id: str) -> bool:
    """First-party status is granted only to ids on the explicit allowlist.

    First-party status unlocks the ``inline`` GCS isolation level and
    the ``inprocess`` agent isolation level. Third parties cannot use
    either even if they declare them in the manifest.
    """
    return signer_id in FIRST_PARTY_SIGNERS
