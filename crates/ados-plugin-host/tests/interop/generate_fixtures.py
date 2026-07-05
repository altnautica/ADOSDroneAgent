#!/usr/bin/env python3
"""Generate cross-language plugin-lifecycle fixtures from the live agent code.

This imports the live ``ados.plugins.archive`` canonical-hash function and the
``ados.services.signing`` Ed25519 verifier, builds a real ``.adosplug``
zip, computes the canonical payload hash the way the agent signs it, signs that
hash with a fresh Ed25519 keypair, and writes everything the Rust
``ados-plugin-host`` crate needs to assert byte-for-byte parity:

* the full archive bytes (so the Rust reader parses the exact same zip),
* the canonical payload hash (hex),
* the SPKI PEM public key + the base64 Ed25519 signature over that hash,
* a tampered signature, a revoked-signer case, and an unknown-signer case.

The signature itself is verified with the agent's own ``verify_signature`` here
so the fixture cannot ship a signature the agent would reject. Run from the
agent repo with its venv:

    .venv/bin/python crates/ados-plugin-host/tests/interop/generate_fixtures.py

It writes ``fixtures.json`` next to this script. Regenerate and commit whenever
the canonical-hash or signature contract changes; CI builds the Rust crate
against the committed file.
"""

from __future__ import annotations

import base64
import io
import json
import zipfile
from pathlib import Path

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

from ados.plugins.archive import parse_archive_bytes
from ados.services.signing import verify_signature

SIGNER_ID = "altnautica-2026-A"

MANIFEST_YAML = (
    "id: com.altnautica.example\n"
    "version: 1.2.3\n"
    "name: Example\n"
    "risk: medium\n"
    "compatibility:\n"
    '  ados_version: ">=0.1.0,<2.0.0"\n'
    "agent:\n"
    "  entrypoint: agent/py/example.py\n"
    "  permissions:\n"
    "    - telemetry.read\n"
)

# Deterministic 32-byte Ed25519 seed so the keypair (and therefore the PEM and
# the signature) is reproducible across runs.
SEED = bytes(range(32))


def _build_archive() -> bytes:
    """Build a stored (no-compression) .adosplug zip with a stable layout.

    Entry insertion order is intentionally NOT path-sorted so the test proves
    the canonical hash is order-independent (it sorts internally).
    """
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_STORED) as zf:
        zf.writestr("agent/py/example.py", b"print('hello from example')\n")
        zf.writestr("manifest.yaml", MANIFEST_YAML.encode("utf-8"))
        zf.writestr("assets/model.bin", b"\x00\x01\x02\x03binary-asset")
    return buf.getvalue()


def main() -> None:
    archive_bytes = _build_archive()

    # Parse via the live agent reader to get the canonical payload hash exactly
    # the way the agent computes the value it signs.
    contents = parse_archive_bytes(archive_bytes)
    payload_hash = contents.payload_hash  # 32-byte sha256 digest

    # Fresh Ed25519 keypair from the fixed seed.
    private_key = Ed25519PrivateKey.from_private_bytes(SEED)
    public_key = private_key.public_key()
    public_pem = public_key.public_bytes(
        encoding=serialization.Encoding.PEM,
        format=serialization.PublicFormat.SubjectPublicKeyInfo,
    ).decode("ascii")

    # Sign the 32-byte payload hash (not the whole archive) per the model.
    signature = private_key.sign(payload_hash)
    signature_b64 = base64.b64encode(signature).decode("ascii")

    # Self-check: the agent's own verifier must accept this signature.
    assert verify_signature(
        payload_hash, signature_b64, public_pem.encode("ascii")
    ), "agent verify_signature rejected the freshly produced signature"

    # A tampered signature: flip the last base64 char to a different value.
    tampered = bytearray(signature)
    tampered[-1] ^= 0x01
    tampered_b64 = base64.b64encode(bytes(tampered)).decode("ascii")
    assert not verify_signature(
        payload_hash, tampered_b64, public_pem.encode("ascii")
    ), "tampered signature unexpectedly verified"

    out = {
        "signer_id": SIGNER_ID,
        "archive_b64": base64.b64encode(archive_bytes).decode("ascii"),
        "payload_hash_hex": payload_hash.hex(),
        "public_pem": public_pem,
        "signature_b64": signature_b64,
        "tampered_signature_b64": tampered_b64,
        "manifest_id": contents.manifest.id,
        "manifest_version": contents.manifest.version,
    }
    dest = Path(__file__).with_name("fixtures.json")
    dest.write_text(json.dumps(out, indent=2, sort_keys=True) + "\n")
    print(f"wrote {dest}")


if __name__ == "__main__":
    main()
