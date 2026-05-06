#!/usr/bin/env bats
# =============================================================================
# Bats test suite for the lightweight agent installer's target dispatcher.
#
# Tests the (arch, libc) -> Rust target triple table by invoking the
# sourceable library at scripts/lib/detect-target.sh with mocked
# uname / ldd output. Six combos are exercised; the unsupported pair
# (armv7l + glibc) is asserted to fail with a non-zero exit and a
# readable error.
# =============================================================================

setup() {
    REPO_ROOT="$(cd "$(dirname "${BATS_TEST_FILENAME}")/../.." && pwd)"
    DETECT_LIB="${REPO_ROOT}/scripts/lib/detect-target.sh"
    [ -f "${DETECT_LIB}" ] || {
        echo "missing dispatcher library: ${DETECT_LIB}" >&2
        return 1
    }
    # Make sure no stale mock vars leak from the parent shell.
    unset ADOS_MOCK_ARCH
    unset ADOS_MOCK_LIBC
}

# -----------------------------------------------------------------------------
# Supported combos
# -----------------------------------------------------------------------------

@test "armv7l + musl picks armv7-musleabihf artifact" {
    run env ADOS_MOCK_ARCH=armv7l ADOS_MOCK_LIBC=musl bash "${DETECT_LIB}" --print-target
    [ "$status" -eq 0 ]
    [[ "$output" == *"armv7-unknown-linux-musleabihf"* ]]
}

@test "aarch64 + musl picks aarch64-musl artifact" {
    run env ADOS_MOCK_ARCH=aarch64 ADOS_MOCK_LIBC=musl bash "${DETECT_LIB}" --print-target
    [ "$status" -eq 0 ]
    [[ "$output" == *"aarch64-unknown-linux-musl"* ]]
}

@test "aarch64 + glibc picks aarch64-gnu artifact" {
    run env ADOS_MOCK_ARCH=aarch64 ADOS_MOCK_LIBC=glibc bash "${DETECT_LIB}" --print-target
    [ "$status" -eq 0 ]
    [[ "$output" == *"aarch64-unknown-linux-gnu"* ]]
}

@test "x86_64 + musl picks x86_64-musl artifact" {
    run env ADOS_MOCK_ARCH=x86_64 ADOS_MOCK_LIBC=musl bash "${DETECT_LIB}" --print-target
    [ "$status" -eq 0 ]
    [[ "$output" == *"x86_64-unknown-linux-musl"* ]]
}

@test "x86_64 + glibc falls back to x86_64-musl artifact" {
    run env ADOS_MOCK_ARCH=x86_64 ADOS_MOCK_LIBC=glibc bash "${DETECT_LIB}" --print-target
    [ "$status" -eq 0 ]
    # No glibc x86_64 artifact is published; the musl variant is
    # statically linked and runs on glibc hosts too.
    [[ "$output" == *"x86_64-unknown-linux-musl"* ]]
}

# -----------------------------------------------------------------------------
# Unsupported combos
# -----------------------------------------------------------------------------

@test "armv7l + glibc is unsupported and exits non-zero" {
    run env ADOS_MOCK_ARCH=armv7l ADOS_MOCK_LIBC=glibc bash "${DETECT_LIB}" --print-target
    [ "$status" -ne 0 ]
    [[ "$output" == *"unsupported target"* ]]
    [[ "$output" == *"armv7l"* ]]
    [[ "$output" == *"glibc"* ]]
}

@test "completely unknown arch is unsupported and exits non-zero" {
    run env ADOS_MOCK_ARCH=ppc64le ADOS_MOCK_LIBC=glibc bash "${DETECT_LIB}" --print-target
    [ "$status" -ne 0 ]
    [[ "$output" == *"unsupported target"* ]]
}

# -----------------------------------------------------------------------------
# Mock-override surface area
# -----------------------------------------------------------------------------

@test "ADOS_MOCK_ARCH alone overrides uname" {
    run env ADOS_MOCK_ARCH=aarch64 bash "${DETECT_LIB}" --arch
    [ "$status" -eq 0 ]
    [ "$output" = "aarch64" ]
}

@test "ADOS_MOCK_LIBC alone overrides ldd probe" {
    run env ADOS_MOCK_LIBC=musl bash "${DETECT_LIB}" --libc
    [ "$status" -eq 0 ]
    [ "$output" = "musl" ]
}

@test "--resolve takes explicit arch + libc and prints the triple" {
    run bash "${DETECT_LIB}" --resolve aarch64 glibc
    [ "$status" -eq 0 ]
    [ "$output" = "aarch64-unknown-linux-gnu" ]
}

@test "--resolve rejects unsupported pairs" {
    run bash "${DETECT_LIB}" --resolve armv7l glibc
    [ "$status" -ne 0 ]
    [[ "$output" == *"unsupported target"* ]]
}

@test "--help is documented and exits zero" {
    run bash "${DETECT_LIB}" --help
    [ "$status" -eq 0 ]
    [[ "$output" == *"detect-target.sh"* ]]
    [[ "$output" == *"ADOS_MOCK_ARCH"* ]]
}

@test "unknown flag exits with code 2" {
    run bash "${DETECT_LIB}" --bogus-flag
    [ "$status" -eq 2 ]
}
