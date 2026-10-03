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
#   scripts/check-version-bumped.sh <base>    # default: origin/main
#
# <base> is a ref or commit SHA. CI passes the pull request's base SHA, or on a
# push the SHA main pointed at before the push (`github.event.before`): on a
# push, origin/main already equals HEAD, so diffing against it checks nothing.
# An all-zero SHA (the before-SHA of a newly created ref) means "no previous
# commit on this ref" and is checked against HEAD~1. A base that is no longer
# reachable (a force-push rewrote it) is also checked against HEAD~1, so the
# gate never passes without comparing anything.
set -uo pipefail

BASE="${1:-origin/main}"
if [ -z "${BASE//0/}" ]; then
    BASE="HEAD~1"
fi
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

if ! git rev-parse --verify --quiet "${BASE}^{commit}" >/dev/null 2>&1; then
    echo "base ref ${BASE} not found (force-push or shallow clone); checking against HEAD~1" >&2
    BASE="HEAD~1"
    if ! git rev-parse --verify --quiet "${BASE}^{commit}" >/dev/null 2>&1; then
        echo "ERROR: neither the base ref nor HEAD~1 is available; fetch full history (fetch-depth: 0)" >&2
        exit 2
    fi
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

before=$(git show "${BASE}:${VERSION_FILE}" 2>/dev/null | sed -n 's/^__version__[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
after=$(sed -n 's/^__version__[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$VERSION_FILE" | head -1)

if [ -z "$after" ]; then
    echo "ERROR: could not read a version from ${VERSION_FILE}" >&2
    exit 2
fi
if ! [[ "$after" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "ERROR: ${VERSION_FILE} version '${after}' is not MAJOR.MINOR.PATCH" >&2
    exit 1
fi
if [ -z "$before" ]; then
    echo "no version in ${VERSION_FILE} at ${BASE}; nothing to compare against"
    exit 0
fi

# `sort -V` silently degrades to a lexicographic sort on a build without
# version sorting, which would order 0.9.0 above 0.10.0. Refuse to judge then.
if [ "$(printf '0.9.0\n0.10.0\n' | sort -V | tail -1)" != "0.10.0" ]; then
    echo "ERROR: sort -V is not version-sorting on this host; cannot compare versions" >&2
    exit 2
fi
highest=$(printf '%s\n%s\n' "$before" "$after" | sort -V | tail -1)

if [ "$before" = "$after" ] || [ "$highest" != "$after" ]; then
    cat >&2 <<EOF
ERROR: shipped behaviour changed but ${VERSION_FILE} did not move up.

  version at ${BASE}: ${before}
  version now:        ${after}

An installed agent reports this version to \`ados update\`, to the heartbeat,
and in its install result. A version that stays the same or goes down makes an
updated box indistinguishable from one that never updated, and every one of
those surfaces fails silently rather than loudly.

Raise it, or move the change under a path that ships nothing (docs, tests,
tools).
EOF
    exit 1
fi

echo "version bumped: ${before} -> ${after}"
