#!/usr/bin/env bash
# =============================================================================
# Target architecture + libc detection for the lightweight agent installer.
#
# Sourceable library. Exposes:
#   detect_target_arch          — prints uname -m, honors ADOS_MOCK_ARCH
#   detect_target_libc          — prints "musl" or "glibc", honors ADOS_MOCK_LIBC
#   resolve_release_target      — prints the Rust target triple for an
#                                 (arch, libc) pair, or exits 1 with a
#                                 readable error on unsupported combos
#   detect_target               — convenience wrapper: arch -> libc ->
#                                 resolve, prints the final triple
#
# Mock overrides (for tests):
#   ADOS_MOCK_ARCH=<value>      — substitutes for `uname -m`
#   ADOS_MOCK_LIBC=<musl|glibc> — substitutes for the ldd-based libc probe
#
# This file is intentionally side-effect-free when sourced. It defines
# functions only; nothing runs at source time. Direct invocation
# (`bash detect-target.sh`) prints the resolved triple to stdout.
#
# Keep in sync with the dispatcher in scripts/install-lite.sh. The two
# call sites must agree on the supported (arch, libc) → triple table.
# =============================================================================

# When sourced under `set -euo pipefail` the caller's shell options are
# inherited; we do not toggle them here so the library plays nicely with
# whichever script sources it.

detect_target_arch() {
    if [ -n "${ADOS_MOCK_ARCH:-}" ]; then
        printf '%s\n' "${ADOS_MOCK_ARCH}"
        return 0
    fi
    uname -m
}

detect_target_libc() {
    if [ -n "${ADOS_MOCK_LIBC:-}" ]; then
        printf '%s\n' "${ADOS_MOCK_LIBC}"
        return 0
    fi
    # Detect musl by checking ldd output. Musl's ldd prints "musl libc"
    # on the first line; glibc prints "ldd (...)".
    if ldd --version 2>&1 | head -n1 | grep -qi musl; then
        printf 'musl\n'
    else
        printf 'glibc\n'
    fi
}

resolve_release_target() {
    local arch="$1" libc="$2"
    case "${arch}-${libc}" in
        armv7l-musl)
            printf 'armv7-unknown-linux-musleabihf\n'
            ;;
        armv7l-glibc)
            # No glibc armv7 release artifact published. Fail loudly so
            # the operator picks the musl image or files an issue.
            printf 'unsupported target: arch=%s, libc=%s\n' "${arch}" "${libc}" >&2
            return 1
            ;;
        aarch64-glibc)
            printf 'aarch64-unknown-linux-gnu\n'
            ;;
        aarch64-musl)
            printf 'aarch64-unknown-linux-musl\n'
            ;;
        x86_64-musl)
            printf 'x86_64-unknown-linux-musl\n'
            ;;
        x86_64-glibc)
            # No glibc x86_64 release artifact today; the musl variant
            # is statically linked and runs on glibc hosts too.
            printf 'x86_64-unknown-linux-musl\n'
            ;;
        *)
            printf 'unsupported target: arch=%s, libc=%s\n' "${arch}" "${libc}" >&2
            return 1
            ;;
    esac
}

detect_target() {
    local arch libc
    arch="$(detect_target_arch)"
    libc="$(detect_target_libc)"
    resolve_release_target "${arch}" "${libc}"
}

# Allow direct invocation: `bash detect-target.sh` prints the resolved
# triple. Useful for ad-hoc checks and as a wrapper target for the bats
# suite. Sourced callers see no side effects because BASH_SOURCE differs
# from $0 in that case.
if [ "${BASH_SOURCE[0]:-$0}" = "$0" ]; then
    set -euo pipefail
    case "${1:-}" in
        --arch)
            detect_target_arch
            ;;
        --libc)
            detect_target_libc
            ;;
        --resolve)
            shift
            [ $# -ge 2 ] || { echo "usage: $0 --resolve ARCH LIBC" >&2; exit 2; }
            resolve_release_target "$1" "$2"
            ;;
        ""|--print-target)
            detect_target
            ;;
        -h|--help)
            cat <<'EOF'
detect-target.sh — print the Rust target triple for the host (or a mocked host).

Usage:
  detect-target.sh                    print the resolved triple
  detect-target.sh --print-target     same as above
  detect-target.sh --arch             print the detected architecture
  detect-target.sh --libc             print the detected libc flavor
  detect-target.sh --resolve ARCH LIBC
                                      print the triple for an explicit pair

Environment:
  ADOS_MOCK_ARCH   substitute for uname -m
  ADOS_MOCK_LIBC   substitute for the ldd libc probe (musl|glibc)
EOF
            ;;
        *)
            echo "unknown flag: ${1}" >&2
            exit 2
            ;;
    esac
fi
