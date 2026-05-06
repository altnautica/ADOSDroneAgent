#!/usr/bin/env bash
# =============================================================================
# Minisign verify smoke test.
#
# Generates an ephemeral signing keypair, signs a test artifact, and
# confirms minisign verifies it. Then tampers with the artifact and
# asserts the verify call fails with a non-zero exit. This guards the
# release-artifact verification path the lightweight installer relies
# on: if the host's minisign is broken or behaves differently than the
# installer expects, this test catches it before a release is cut.
# =============================================================================

set -euo pipefail

if ! command -v minisign >/dev/null 2>&1; then
    cat <<'EOF' >&2
minisign is not installed.

Install via:
  apt-get install -y minisign        (Debian/Ubuntu)
  apk add minisign                   (Alpine/Buildroot)
  brew install minisign              (macOS)

Or download the prebuilt binary from the upstream release page.
EOF
    exit 127
fi

TMPDIR="$(mktemp -d)"
trap 'rm -rf "${TMPDIR}"' EXIT

cd "${TMPDIR}"

echo "test artifact contents" > artifact.tar.gz

# Generate an unencrypted keypair (-W skips the password prompt).
# Pipe an empty stdin so any residual prompt resolves quickly.
minisign -G -W -p pub.key -s sec.key </dev/null >/dev/null

# Sign the artifact. -W signs without a password (matching the -G).
minisign -S -W -s sec.key -m artifact.tar.gz </dev/null >/dev/null

if [ ! -f artifact.tar.gz.minisig ]; then
    echo "FAIL: signing did not produce a .minisig file" >&2
    exit 1
fi

# Happy path: verify with the matching public key.
if ! minisign -V -p pub.key -m artifact.tar.gz >/dev/null 2>&1; then
    echo "FAIL: untampered artifact failed signature verification" >&2
    exit 1
fi

# Tamper path: append a byte and assert verify rejects it.
echo "tampered" >> artifact.tar.gz
if minisign -V -p pub.key -m artifact.tar.gz >/dev/null 2>&1; then
    echo "FAIL: tampered artifact verified successfully (should have failed)" >&2
    exit 1
fi

# Wrong-key path: generate a second keypair and assert that key cannot
# verify a signature produced by the first. Catches a class of release
# pipeline bugs where the wrong key is embedded in the installer.
echo "test artifact contents" > artifact.tar.gz   # restore clean copy
minisign -S -W -s sec.key -m artifact.tar.gz </dev/null >/dev/null
minisign -G -W -f -p pub2.key -s sec2.key </dev/null >/dev/null
if minisign -V -p pub2.key -m artifact.tar.gz >/dev/null 2>&1; then
    echo "FAIL: signature verified with the wrong public key" >&2
    exit 1
fi

echo "ok: minisign verify path is sound (happy + tamper + wrong-key)"
