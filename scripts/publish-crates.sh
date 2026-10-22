#!/usr/bin/env bash
# Publish every library crate version that crates.io does not have yet.
#
# The publishable set and each crate's version come from `cargo metadata`
# (members without `publish = false`). A `name@version` is missing when the
# crates.io sparse index has no entry for the crate, or no line whose "vers"
# is that version. All missing crates go to one `cargo publish`, which orders
# interdependent packages itself (ados-protocol before ados-sdk and
# ados-vision). Already-published versions are skipped, so the script is
# safe to rerun and does nothing when nothing was bumped.
#
# After a real publish each crate is tagged `crates/<name>-v<version>` and the
# tags are pushed; scripts/check-crates.sh reads those tags for its bump check.
#
# Usage:
#   scripts/publish-crates.sh             # publish, tag, push tags
#   scripts/publish-crates.sh --dry-run   # cargo publish --dry-run, no tags
#   scripts/publish-crates.sh --no-tags   # publish, print the git tag commands
#
# Auth: CARGO_REGISTRY_TOKEN (Trusted Publishing in CI, a short-lived token for
# a crate's first release).
set -euo pipefail

dry_run=0
tags=1
for arg in "$@"; do
    case "$arg" in
        --dry-run) dry_run=1 ;;
        --no-tags) tags=0 ;;
        *) echo "usage: publish-crates.sh [--dry-run] [--no-tags]" >&2; exit 64 ;;
    esac
done

cd "$(dirname "$0")/../crates"

index_path() {
    # crates.io sparse index layout: 1/, 2/, 3/<c1>/, else <c1c2>/<c3c4>/.
    local n
    n="$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')"
    case "${#n}" in
        1) printf '1/%s' "$n" ;;
        2) printf '2/%s' "$n" ;;
        3) printf '3/%s/%s' "${n:0:1}" "$n" ;;
        *) printf '%s/%s/%s' "${n:0:2}" "${n:2:2}" "$n" ;;
    esac
}

is_published() {
    # is_published <name> <version>: 0 when the index lists that version.
    local body code
    body="$(mktemp)"
    code="$(curl -sS -A 'ados-publish-crates (github.com/altnautica/ADOSDroneAgent)' \
        -o "$body" -w '%{http_code}' "https://index.crates.io/$(index_path "$1")")"
    case "$code" in
        200) grep -Fq "\"vers\":\"$2\"" "$body"; local rc=$?; rm -f "$body"; return "$rc" ;;
        404) rm -f "$body"; return 1 ;;
        *) rm -f "$body"; echo "ERROR: crates.io index returned HTTP $code for $1" >&2; exit 2 ;;
    esac
}

meta="$(cargo metadata --no-deps --format-version 1)"
missing=()
while read -r name version; do
    if is_published "$name" "$version"; then
        echo "published: $name $version"
    else
        echo "missing:   $name $version"
        missing+=("$name@$version")
    fi
done < <(jq -r '.packages[] | select(.publish == null) | "\(.name) \(.version)"' <<<"$meta" | tr -d '\r' | sort)

if [ "${#missing[@]}" -eq 0 ]; then
    echo "nothing to publish"
    exit 0
fi

args=(--locked)
for spec in "${missing[@]}"; do args+=(-p "${spec%@*}"); done
[ "$dry_run" -eq 1 ] && args+=(--dry-run)

echo "cargo publish ${args[*]}"
cargo publish "${args[@]}"

[ "$dry_run" -eq 1 ] && exit 0

tag_names=()
for spec in "${missing[@]}"; do tag_names+=("crates/${spec%@*}-v${spec#*@}"); done

if [ "$tags" -eq 0 ]; then
    echo "published; tag the published commit with:"
    for t in "${tag_names[@]}"; do echo "  git tag $t"; done
    echo "  git push origin ${tag_names[*]}"
    exit 0
fi

for t in "${tag_names[@]}"; do git tag "$t"; done
git push origin "${tag_names[@]}"
echo "tagged: ${tag_names[*]}"
