#!/usr/bin/env bash
# =============================================================================
# verify.sh smoke test (full agent).
#
# Exercises scripts/lib/verify.sh end to end with an ephemeral minisign
# keypair: a clean artifact verifies, a SHA256 tamper is rejected, a
# signature tamper is rejected (with SHA256 still passing so the invalid
# signature is what trips it), the wrong key is rejected, a missing
# signature is tolerated on edge but refused on stable, and the explicit
# allow-unsigned bypass works. Guards the artifact-verification path the
# full agent's prebuilt-module and stable-channel installs rely on.
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/lib/verify.sh
. "${SCRIPT_DIR}/../lib/verify.sh"

if ! command -v minisign >/dev/null 2>&1; then
    echo "minisign is not installed; install via apt-get/apk/brew install minisign" >&2
    exit 127
fi

TMPDIR="$(mktemp -d)"
trap 'rm -rf "${TMPDIR}"' EXIT
cd "${TMPDIR}"

fail() { echo "FAIL: $*" >&2; exit 1; }

# --- fixtures -------------------------------------------------------------
echo "ados artifact payload" > art.bin
minisign -G -W -p pub.key -s sec.key </dev/null >/dev/null
minisign -S -W -s sec.key -m art.bin </dev/null >/dev/null
sha256sum art.bin > art.bin.sha256
PUBKEY="$(tail -n1 pub.key)"     # base64 key line of a minisign pub key file
[ -n "${PUBKEY}" ] || fail "could not extract public key string"

# Second keypair for the wrong-key case.
minisign -G -W -f -p pub2.key -s sec2.key </dev/null >/dev/null
PUBKEY2="$(tail -n1 pub2.key)"

# --- 1. happy path: valid sha256 + valid signature -----------------------
ados_verify_artifact art.bin "${PUBKEY}" edge   || fail "clean artifact rejected (edge)"
ados_verify_artifact art.bin "${PUBKEY}" stable || fail "clean artifact rejected (stable)"

# --- 2. sha256 tamper: must be rejected before the signature even matters -
cp art.bin art2.bin; cp art.bin.minisig art2.bin.minisig
printf 'x' >> art2.bin                       # corrupt payload
cp art.bin.sha256 art2.bin.sha256            # stale sum now mismatches
sed -i.bak 's/art\.bin/art2.bin/' art2.bin.sha256 2>/dev/null || \
    sed 's/art\.bin/art2.bin/' art.bin.sha256 > art2.bin.sha256
if ados_verify_artifact art2.bin "${PUBKEY}" edge; then
    fail "sha256-tampered artifact accepted"
fi

# --- 3. signature tamper: sha256 PASSES, signature is stale/invalid -------
# Re-checksum the corrupted payload so sha256 passes, but keep the OLD
# signature so the minisign check trips. Must be fatal on EVERY channel.
cp art2.bin sigtamper.bin
sha256sum sigtamper.bin > sigtamper.bin.sha256
cp art.bin.minisig sigtamper.bin.minisig     # signature of the original, not this payload
ados_verify_sha256 sigtamper.bin || fail "test setup: re-summed payload should pass sha256"
if ados_verify_artifact sigtamper.bin "${PUBKEY}" edge; then
    fail "signature-tampered artifact accepted on edge (must be fatal)"
fi
if ados_verify_artifact sigtamper.bin "${PUBKEY}" stable; then
    fail "signature-tampered artifact accepted on stable"
fi

# --- 4. wrong key: valid signature, wrong public key -> tamper -> fatal ---
if ados_verify_artifact art.bin "${PUBKEY2}" edge; then
    fail "artifact verified with the wrong public key"
fi

# --- 5. unverifiable (no .minisig): edge tolerates, stable refuses --------
cp art.bin nosig.bin; sha256sum nosig.bin > nosig.bin.sha256
ados_verify_artifact nosig.bin "${PUBKEY}" edge   || fail "missing-sig should be tolerated on edge"
if ados_verify_artifact nosig.bin "${PUBKEY}" stable; then
    fail "missing-sig accepted on stable (must refuse)"
fi

# --- 6. explicit allow-unsigned bypass (sha256 still enforced) ------------
ados_verify_artifact nosig.bin "${PUBKEY}" stable 1 || fail "allow-unsigned bypass failed"
if ados_verify_artifact art2.bin "${PUBKEY}" edge 1; then
    fail "allow-unsigned must still enforce sha256"
fi

echo "ok: verify.sh sound (happy + sha256-tamper + sig-tamper + wrong-key + missing-sig + allow-unsigned)"
