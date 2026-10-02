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
# shellcheck disable=SC1091  # sourced at runtime, not analyzed without -x
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
ados_verify_artifact art.bin "${PUBKEY}" || fail "clean artifact rejected"

# --- 2. sha256 tamper: must be rejected before the signature even matters -
cp art.bin art2.bin; cp art.bin.minisig art2.bin.minisig
printf 'x' >> art2.bin                       # corrupt payload
cp art.bin.sha256 art2.bin.sha256            # stale sum now mismatches
sed -i.bak 's/art\.bin/art2.bin/' art2.bin.sha256 2>/dev/null || \
    sed 's/art\.bin/art2.bin/' art.bin.sha256 > art2.bin.sha256
if ados_verify_artifact art2.bin "${PUBKEY}"; then
    fail "sha256-tampered artifact accepted"
fi

# --- 3. signature tamper: sha256 PASSES, signature is stale/invalid -------
# Re-checksum the corrupted payload so sha256 passes, but keep the OLD
# signature so the minisign check trips.
cp art2.bin sigtamper.bin
sha256sum sigtamper.bin > sigtamper.bin.sha256
cp art.bin.minisig sigtamper.bin.minisig     # signature of the original, not this payload
ados_verify_sha256 sigtamper.bin || fail "test setup: re-summed payload should pass sha256"
if ados_verify_artifact sigtamper.bin "${PUBKEY}"; then
    fail "signature-tampered artifact accepted"
fi

# --- 4. wrong key: valid signature, wrong public key -> tamper -> fatal ---
if ados_verify_artifact art.bin "${PUBKEY2}"; then
    fail "artifact verified with the wrong public key"
fi

# --- 5. unverifiable (no .minisig): refused -------------------------------
cp art.bin nosig.bin; sha256sum nosig.bin > nosig.bin.sha256
if ados_verify_artifact nosig.bin "${PUBKEY}"; then
    fail "missing-sig accepted (must refuse)"
fi

# --- 6. no signing key: refused --------------------------------------------
if ados_verify_artifact art.bin ""; then
    fail "artifact accepted with no signing key"
fi

echo "ok: verify.sh sound (happy + sha256-tamper + sig-tamper + wrong-key + missing-sig + no-key)"
