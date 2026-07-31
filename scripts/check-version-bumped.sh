#!/usr/bin/env bash
# Fail when shipped behaviour changed without a version bump.
#
# Twelve commits of shipped behaviour once went out with the version unchanged.
# The consequences are all silent: `ados update` compares the version off the
# tip of main and reports a stale box as up to date; the heartbeat tells the
# fleet the wrong version; the install-result contract and the OpenAPI version
# both misreport. Nothing fails loudly, so nobody notices until a box cannot be
# told apart from one that never updated.
#
# Usage:
#   scripts/check-version-bumped.sh <base-ref>    # default: origin/main
set -uo pipefail

BASE="${1:-origin/main}"
VERSION_FILE="src/ados/__init__.py"

# Paths whose change means shipped behaviour changed. Deliberately broad: it is
# far cheaper to bump a version unnecessarily than to ship an indistinguishable
# box. Docs, tests and bench tooling are excluded because they change nothing an
# installed agent does.
is_shipped_path() {
    case "$1" in
        docs/* | *.md | tests/* | tools/* | .github/* | scripts/test/*) return 1 ;;
        src/* | crates/* | data/* | scripts/* | cockpit/* | dashboard/*) return 0 ;;
        *) return 1 ;;
    esac
}

if ! git rev-parse --verify "$BASE" >/dev/null 2>&1; then
    echo "base ref ${BASE} not found; skipping (shallow clone or first push)" >&2
    exit 0
fi

changed=$(git diff --name-only "${BASE}...HEAD") || {
    echo "ERROR: could not diff against ${BASE}" >&2
    exit 2
}
[ -n "$changed" ] || { echo "no changes against ${BASE}"; exit 0; }

shipped=0
while IFS= read -r f; do
    [ -n "$f" ] || continue
    if is_shipped_path "$f"; then
        shipped=1
        break
    fi
done <<< "$changed"

if [ "$shipped" -eq 0 ]; then
    echo "no shipped-behaviour paths changed; version bump not required"
    exit 0
fi

before=$(git show "${BASE}:${VERSION_FILE}" 2>/dev/null | grep -oE '__version__ = "[^"]+"' | head -1)
after=$(grep -oE '__version__ = "[^"]+"' "$VERSION_FILE" | head -1)

if [ -z "$after" ]; then
    echo "ERROR: could not read a version from ${VERSION_FILE}" >&2
    exit 2
fi

if [ "$before" = "$after" ]; then
    cat >&2 <<EOF
ERROR: shipped behaviour changed but ${VERSION_FILE} did not.

  version: ${after#__version__ = }

An installed agent reports this version to \`ados update\`, to the heartbeat,
and in its install result. Leaving it unchanged makes an updated box
indistinguishable from one that never updated — and every one of those
surfaces fails silently rather than loudly.

Bump it, or move the change under a path that ships nothing (docs, tests,
tools).
EOF
    exit 1
fi

echo "version bumped: ${before#__version__ = } -> ${after#__version__ = }"
