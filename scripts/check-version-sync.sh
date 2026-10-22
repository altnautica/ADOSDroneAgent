#!/usr/bin/env bash
# Fail when the Rust workspace version and the Python agent version disagree.
#
# Every Rust binary reports `CARGO_PKG_VERSION` (the `[workspace.package]`
# version in crates/Cargo.toml) to the cloud heartbeat, the plugin-host semver
# gate and the GCS compatibility checks, while the wheel, the installer and
# `ados update` use `__version__` from src/ados/__init__.py. Two numbers for one
# agent means one of those surfaces reports a version that was never shipped.
#
# Usage:
#   scripts/check-version-sync.sh            # the two sources must agree
#   scripts/check-version-sync.sh <version>  # ...and both must equal <version>
#
# <version> may carry a leading `v` (a release tag such as v1.2.3 is accepted
# as-is), so the release workflow can pass the pushed tag straight through.
set -uo pipefail

cd "$(dirname "$0")/.." || exit 2

CARGO_FILE="crates/Cargo.toml"
PY_FILE="src/ados/__init__.py"

# The version line inside [workspace.package], not the first `version =` in the
# file: a dependency table further down also carries `version` keys.
cargo_version=$(awk '
    /^\[/ { in_pkg = ($0 == "[workspace.package]") ; next }
    in_pkg && /^version[[:space:]]*=/ {
        line = $0
        sub(/^version[[:space:]]*=[[:space:]]*"/, "", line)
        sub(/".*$/, "", line)
        print line
        exit
    }
' "$CARGO_FILE")

py_version=$(sed -n 's/^__version__[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$PY_FILE" | head -1)

if [ -z "$cargo_version" ]; then
    echo "ERROR: no version under [workspace.package] in ${CARGO_FILE}" >&2
    exit 2
fi
if [ -z "$py_version" ]; then
    echo "ERROR: no __version__ in ${PY_FILE}" >&2
    exit 2
fi

fail=0
if [ "$cargo_version" != "$py_version" ]; then
    echo "FAIL: ${CARGO_FILE} workspace version is ${cargo_version}, ${PY_FILE} __version__ is ${py_version}." >&2
    echo "  Set both to the same version in the same commit." >&2
    fail=1
fi

# The lockstep published crates (ados-protocol and the crates that depend on
# it) are pinned in [workspace.dependencies] with an exact `version = "=X.Y.Z"`,
# and that pin is the requirement a published dependent records. A pin that
# drifts from the workspace version publishes a crate that asks for a wire
# version nobody ships. Every exact pin in that table is checked; ados-protocol
# must be among them, so an emptied table cannot pass vacuously.
pins=$(awk '
    /^\[/ { in_deps = ($0 == "[workspace.dependencies]") ; next }
    in_deps && match($0, /version[[:space:]]*=[[:space:]]*"=[^"]*"/) {
        v = substr($0, RSTART, RLENGTH)
        sub(/^version[[:space:]]*=[[:space:]]*"=/, "", v)
        sub(/"$/, "", v)
        name = $0
        sub(/[[:space:]]*=.*/, "", name)
        print name, v
    }
' "$CARGO_FILE")
if ! printf '%s\n' "$pins" | grep -q '^ados-protocol '; then
    echo "FAIL: ${CARGO_FILE} [workspace.dependencies] has no exact ados-protocol pin (version = \"=${cargo_version}\")." >&2
    fail=1
fi
while read -r name pin; do
    [ -n "$name" ] || continue
    if [ "$pin" != "$cargo_version" ]; then
        echo "FAIL: ${CARGO_FILE} [workspace.dependencies] ${name} pins =${pin}, workspace version is ${cargo_version}. Set them in the same commit." >&2
        fail=1
    fi
done <<<"$pins"

if [ "$#" -ge 1 ]; then
    expected="${1#v}"
    if [ "$py_version" != "$expected" ] || [ "$cargo_version" != "$expected" ]; then
        echo "FAIL: expected version ${expected} (from '$1'), found __version__ ${py_version} and workspace ${cargo_version}." >&2
        echo "  The installer requests the wheel and deploy bundle by the tag's version, so a" >&2
        echo "  release whose tag and package versions differ publishes files no install can fetch." >&2
        fail=1
    fi
fi

if [ "$fail" -eq 0 ]; then
    echo "version in sync: ${py_version}"
fi
exit "$fail"
