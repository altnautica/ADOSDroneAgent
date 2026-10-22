#!/usr/bin/env bash
# Gate for the library crates this workspace publishes to crates.io.
#
# A crate is published when its manifest does not say `publish = false`; the
# set is read from `cargo metadata`, never from a hand-kept list. For that set
# the script checks, in order:
#
#   metadata  licence, description, readme, repository, homepage, documentation,
#             1-5 keywords, 1-5 categories, a README.md, and both LICENSE-*
#             files for the MIT OR Apache-2.0 crates
#   deps      no normal or build dependency on an unpublished ados-* crate, and
#             no ados-* dependency without a registry version
#   package   `cargo package` builds every .crate in isolation, which is what
#             catches an include_str!/include_bytes! path outside the crate dir
#   doc       rustdoc with warnings denied (broken intra-doc links, missing docs)
#   doctest   the README examples, which are each crate's docs.rs front page
#   bump      a crate whose packaged files changed since its
#             `crates/<name>-v<version>` tag must carry a new version
#
# Usage:
#   scripts/check-crates.sh               # every check
#   scripts/check-crates.sh --skip-bump   # all but bump (a tree without git history)
#   scripts/check-crates.sh --bump-only   # only bump (needs git and the crates/* tags)
#
# Linux only for package/doc/doctest: ados-protocol does not build on Windows.
# The master repo's `scripts/linux-gate.sh crates` runs the bump check on the
# host checkout and the rest in the Linux VM.
#
# Exit: 0 every check passed, 1 a check failed, 64 bad usage.
set -euo pipefail

mode="all"
case "${1:-}" in
    '') ;;
    --skip-bump) mode="skip-bump" ;;
    --bump-only) mode="bump-only" ;;
    *) echo "usage: check-crates.sh [--skip-bump|--bump-only]" >&2; exit 64 ;;
esac

for tool in cargo jq git comm; do
    command -v "$tool" >/dev/null 2>&1 || { echo "ERROR: check-crates.sh needs $tool on PATH" >&2; exit 2; }
done

cd "$(dirname "$0")/../crates"

meta="$(cargo metadata --no-deps --format-version 1)"
mapfile -t crates < <(jq -r '.packages[] | select(.publish == null) | .name' <<<"$meta" | sort)
if [ "${#crates[@]}" -eq 0 ]; then
    echo "FAIL: no publishable crate found (every member says publish = false)"
    echo "RESULT: FAIL"
    exit 1
fi
pkg_args=()
for c in "${crates[@]}"; do pkg_args+=(-p "$c"); done
echo "publishable crates (${#crates[@]}): ${crates[*]}"

fail=0
check() {
    # check <name> <command...>: run, report ok/FAIL, never abort the script.
    local name="$1"; shift
    if "$@"; then
        echo "ok: $name"
    else
        echo "FAIL: $name"
        fail=1
    fi
}

check_metadata() {
    local bad=0 c dir lic
    for c in "${crates[@]}"; do
        local problems
        problems="$(jq -r --arg n "$c" '
            .packages[] | select(.name == $n) |
            [ (if (.license // "") == "" then "license" else empty end),
              (if (.description // "") == "" then "description" else empty end),
              (if (.readme // "") == "" then "readme" else empty end),
              (if (.repository // "") == "" then "repository" else empty end),
              (if (.homepage // "") == "" then "homepage" else empty end),
              (if (.documentation // "") == "" then "documentation" else empty end),
              (if (.keywords | length) < 1 or (.keywords | length) > 5 then "keywords (1-5)" else empty end),
              (if (.categories | length) < 1 or (.categories | length) > 5 then "categories (1-5)" else empty end)
            ] | join(", ")' <<<"$meta")"
        if [ -n "$problems" ]; then
            echo "  $c: missing or invalid: $problems"
            bad=1
        fi
        dir="$(jq -r --arg n "$c" '.packages[] | select(.name == $n) | .manifest_path' <<<"$meta")"
        dir="$(dirname "$dir")"
        [ -f "$dir/README.md" ] || { echo "  $c: no README.md"; bad=1; }
        lic="$(jq -r --arg n "$c" '.packages[] | select(.name == $n) | .license' <<<"$meta")"
        if [ "$lic" = "MIT OR Apache-2.0" ]; then
            [ -f "$dir/LICENSE-MIT" ] || { echo "  $c: no LICENSE-MIT"; bad=1; }
            [ -f "$dir/LICENSE-APACHE" ] || { echo "  $c: no LICENSE-APACHE"; bad=1; }
        fi
    done
    return "$bad"
}

check_deps() {
    local out
    out="$(jq -r '
        ([.packages[] | select(.publish != null) | .name]) as $private |
        .packages[] | select(.publish == null) | .name as $n |
        .dependencies[] | select(.name | startswith("ados-")) |
        select(.kind == null or .kind == "build") |
        if (.name as $d | $private | index($d)) then "  \($n) -> \(.name): depends on an unpublished crate"
        elif .req == "*" then "  \($n) -> \(.name): no registry version (use the [workspace.dependencies] entry)"
        else empty end' <<<"$meta")"
    if [ -n "$out" ]; then
        printf '%s\n' "$out"
        return 1
    fi
}

check_bump() {
    local bad=0 c ver tag changed packaged hit
    if ! git rev-parse --git-dir >/dev/null 2>&1; then
        echo "  not a git checkout; the bump check needs the crates/* tags"
        return 1
    fi
    for c in "${crates[@]}"; do
        ver="$(jq -r --arg n "$c" '.packages[] | select(.name == $n) | .version' <<<"$meta")"
        tag="crates/$c-v$ver"
        git rev-parse -q --verify "refs/tags/$tag" >/dev/null || continue
        changed="$(git diff --name-only "$tag" HEAD -- "$c" | sed "s|^crates/$c/||" | sort -u)"
        [ -n "$changed" ] || continue
        packaged="$(cargo package --list --allow-dirty -p "$c" 2>/dev/null | sort -u)"
        hit="$(comm -12 <(printf '%s\n' "$changed") <(printf '%s\n' "$packaged"))"
        if [ -n "$hit" ]; then
            echo "FAIL: $c changed since $ver was published; bump its version"
            printf '    %s\n' $hit
            bad=1
        fi
    done
    return "$bad"
}

if [ "$mode" != "bump-only" ]; then
    check metadata check_metadata
    check deps check_deps
    check package cargo package --locked --allow-dirty "${pkg_args[@]}"
    check doc env RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features "${pkg_args[@]}"
    check doctest cargo test --doc --all-features "${pkg_args[@]}"
fi
if [ "$mode" != "skip-bump" ]; then
    check bump check_bump
fi

if [ "$fail" -eq 0 ]; then
    echo "RESULT: PASS"
else
    echo "RESULT: FAIL"
fi
exit "$fail"
