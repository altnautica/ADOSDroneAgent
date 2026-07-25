#!/usr/bin/env bats
# =============================================================================
# Bats suite for the prebuilt kernel-module lookup in
# scripts/drivers/lib-prebuilt.sh.
#
# The publisher regenerates drivers-manifest.json from only the rows built by
# the current run, while the .ko assets themselves are never pruned. So a
# module for a kernel that upstream has since moved past stays downloadable
# but stops being advertised. Before the direct-asset probe that silently cost
# every such board its fast path and sent it into a multi-minute on-device
# compile.
#
# The invariants under test:
#   1. Manifest row present      -> that row's file is used (unchanged path).
#   2. Manifest row MISSING      -> probe the deterministic
#                                   <module>-<kver>-<arch>.ko and install it.
#   3. Manifest UNREACHABLE      -> same direct probe (a lost manifest must
#                                   not cost the fast path).
#   4. Asset genuinely absent    -> clean failure, nothing installed.
#   5. No sha256 anywhere        -> FAIL CLOSED, never install unverified.
#
# The network, the verifier and every kernel-module command are mocked, so the
# suite runs unprivileged and touches nothing outside a temp tree.
# =============================================================================

setup() {
    REPO_ROOT="$(cd "$(dirname "${BATS_TEST_FILENAME}")/../.." && pwd)"
    LIB="${REPO_ROOT}/scripts/drivers/lib-prebuilt.sh"
    [ -f "${LIB}" ] || {
        echo "missing lib: ${LIB}" >&2
        return 1
    }
    TMP="$(mktemp -d)"
    REMOTE="${TMP}/remote"   # what the "release" serves
    INSTALLED="${TMP}/installed"
    mkdir -p "${REMOTE}" "${INSTALLED}"

    MODULE="8812eu"
    KVER="6.18.33+rpt-rpi-v8"
    KARCH="arm64"
    ASSET="${MODULE}-${KVER}-${KARCH}.ko"

    export ADOS_PREBUILT_BASE_URL="file://${REMOTE}"
    export ADOS_PREBUILT_ALLOW_UNSIGNED=1
    # The running-kernel vermagic compare needs a live in-tree module to read;
    # there is none in this sandbox, so _pb_running_vermagic returns empty and
    # the compare is skipped. Asset selection is what this suite pins.
    export ADOS_PREBUILT_VERMAGIC_STRICT=0
}

teardown() {
    [ -n "${TMP:-}" ] && rm -rf "${TMP}"
}

# Publish a module (and, unless told otherwise, its sha256 sidecar).
publish_asset() {
    local name="$1" with_sha="${2:-yes}"
    printf 'fake-module-bytes\n' > "${REMOTE}/${name}"
    if [ "${with_sha}" = "yes" ]; then
        ( cd "${REMOTE}" && sha256sum "${name}" > "${name}.sha256" )
    fi
}

publish_manifest() {
    printf '%s\n' "$1" > "${REMOTE}/drivers-manifest.json"
}

# Source the lib with every external touchpoint mocked.
load_lib_with_mocks() {
    # shellcheck disable=SC1090
    . "${LIB}"

    info() { echo "INFO: $*"; }
    warn() { echo "WARN: $*"; }

    # Serve from the temp "release" dir; a missing file is a fetch failure.
    ados_fetch() {
        local url="$1" dest="$2"
        local path="${url#file://}"
        [ -f "${path}" ] || return 1
        cp "${path}" "${dest}"
    }
    # Verification is exercised in its own suite; here it always passes so the
    # tests isolate WHICH asset gets selected.
    ados_verify_artifact() { return 0; }

    # Kernel-module surface.
    modinfo() { echo ""; }
    depmod() { return 0; }
    modprobe() { return 0; }
    lsmod() { echo "${MODULE} 1 0"; }
    install() {
        # Record only the final module placement; ignore `install -d`.
        case "$*" in
            *-d*) return 0 ;;
        esac
        local dest="${*##* }"
        echo "${dest}" >> "${INSTALLED}/placements"
        return 0
    }
    export -f 2>/dev/null || true
}

@test "manifest row present: uses the advertised file" {
    publish_asset "${ASSET}"
    publish_manifest '{"drivers":[{"module":"8812eu","kver":"6.18.33+rpt-rpi-v8","arch":"arm64","file":"'"${ASSET}"'","sha256":"x","vermagic":"y"}]}'

    load_lib_with_mocks
    run try_prebuilt_install "${MODULE}" "${KVER}" "${KARCH}"
    [ "${status}" -eq 0 ]
}

@test "manifest row MISSING: falls back to the deterministic asset name" {
    publish_asset "${ASSET}"
    # Manifest advertises only a NEWER kernel — the exact orphaning that
    # happens when upstream bumps and CI regenerates.
    publish_manifest '{"drivers":[{"module":"8812eu","kver":"6.18.34+rpt-rpi-v8","arch":"arm64","file":"8812eu-6.18.34+rpt-rpi-v8-arm64.ko","sha256":"x","vermagic":"y"}]}'

    load_lib_with_mocks
    run try_prebuilt_install "${MODULE}" "${KVER}" "${KARCH}"
    [ "${status}" -eq 0 ]
    grep -q "probing ${ASSET} directly" <<< "${output}"
}

@test "manifest UNREACHABLE: still probes the asset directly" {
    publish_asset "${ASSET}"
    # No manifest published at all.

    load_lib_with_mocks
    run try_prebuilt_install "${MODULE}" "${KVER}" "${KARCH}"
    [ "${status}" -eq 0 ]
    grep -q "probing for the asset directly" <<< "${output}"
}

@test "asset genuinely absent: clean failure, nothing installed" {
    # Nothing published at all.
    load_lib_with_mocks
    run try_prebuilt_install "${MODULE}" "${KVER}" "${KARCH}"
    [ "${status}" -ne 0 ]
    [ ! -f "${INSTALLED}/placements" ]
}

@test "no sha256 anywhere: fails closed rather than installing unverified" {
    publish_asset "${ASSET}" "no"   # module published, sidecar withheld
    # No manifest -> no fallback hash either.

    load_lib_with_mocks
    run try_prebuilt_install "${MODULE}" "${KVER}" "${KARCH}"
    [ "${status}" -ne 0 ]
    grep -q "no sha256 to verify against" <<< "${output}"
    [ ! -f "${INSTALLED}/placements" ]
}
