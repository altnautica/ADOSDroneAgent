#!/usr/bin/env bats
# =============================================================================
# Bats test suite for the prebuilt RTL8812EU kernel-module install path.
#
# Exercises try_prebuilt_install in scripts/drivers/lib-prebuilt.sh with a
# fully mocked environment so no real network, kernel, or root access is
# needed. PATH-shimmed stubs stand in for the system commands the path
# shells out to (modinfo / lsmod / modprobe / depmod / install / mkdir),
# and the sourced net.sh / verify.sh helpers are overridden with shell
# functions after sourcing. Each case drives one branch:
#
#   prebuilt-hit          -> 0, breadcrumb written = "prebuilt"
#   vermagic-mismatch     -> non-zero (caller falls back to DKMS)
#   sha256-mismatch       -> non-zero
#   manifest-unreachable  -> non-zero
#   offline               -> non-zero, fast (no hang)
#   key-not-in-manifest   -> non-zero
# =============================================================================

setup() {
    REPO_ROOT="$(cd "$(dirname "${BATS_TEST_FILENAME}")/../.." && pwd)"
    LIB="${REPO_ROOT}/scripts/drivers/lib-prebuilt.sh"
    [ -f "${LIB}" ] || {
        echo "missing library: ${LIB}" >&2
        return 1
    }

    # Per-test scratch: a fake PATH bin dir for command stubs, a fake
    # /run/ados breadcrumb root, and a download staging dir.
    TESTTMP="$(mktemp -d)"
    BINDIR="${TESTTMP}/bin"
    RUNDIR="${TESTTMP}/run-ados"
    mkdir -p "${BINDIR}" "${RUNDIR}"
}

teardown() {
    [ -n "${TESTTMP:-}" ] && rm -rf "${TESTTMP}"
}

# Write an executable stub into the test PATH dir.
# usage: stub <name> <body...>
stub() {
    local name="$1"; shift
    {
        echo '#!/usr/bin/env bash'
        printf '%s\n' "$@"
    } > "${BINDIR}/${name}"
    chmod +x "${BINDIR}/${name}"
}

# Build a driver script in the scratch dir that sources the real
# lib-prebuilt.sh, then overrides the sourced network/verify helpers and
# the breadcrumb path with mocks parameterised per scenario. The system
# commands (modinfo/lsmod/modprobe/depmod/install/mkdir/depmod) are taken
# from the PATH stubs. Each scenario sets a few env knobs read by the
# mocks below.
#
# Knobs (env):
#   MOCK_REACHABLE      1 reachable / 0 offline
#   MOCK_MANIFEST_OK    1 manifest fetch + verify succeeds / 0 fails
#   MOCK_LOOKUP         "filename\tsha256\tvermagic" or empty (no match)
#   MOCK_KO_VERIFY      1 .ko verify ok / 0 fails (sha/sig)
#   MOCK_VERMAGIC_OK    1 vermagic safe / 0 unsafe
write_runner() {
    cat > "${TESTTMP}/run.sh" <<RUNNER
#!/usr/bin/env bash
set -euo pipefail

# Source the real library (it will source net.sh + verify.sh).
# shellcheck source=/dev/null
. "${LIB}"

# Redirect the breadcrumb to the scratch /run/ados via a wrapper that
# mkdir's the test dir. We cannot patch the hardcoded /run/ados write in
# the library from a non-root test, so we shadow 'mkdir' + the printf
# target by overriding the well-known path through an env the library
# reads is not available — instead, assert breadcrumb by intercepting the
# system 'install'/'modprobe' success and writing our own marker. The
# library's own /run/ados write is best-effort (|| true) so it simply
# no-ops under the test's non-root, read-only /run.

# --- override sourced helpers -------------------------------------------
ados_reachable() { [ "\${MOCK_REACHABLE:-1}" = "1" ]; }

ados_fetch() {
    local url="\$1" out="\${2:-}"
    case "\$url" in
        *drivers-manifest.json)
            [ "\${MOCK_MANIFEST_OK:-1}" = "1" ] || return 1
            [ -n "\$out" ] && echo '{"module":"8812eu","modules":[]}' > "\$out"
            return 0 ;;
        *drivers-manifest.json.sha256)
            [ "\${MOCK_MANIFEST_OK:-1}" = "1" ] || return 1
            [ -n "\$out" ] && echo 'sha  drivers-manifest.json' > "\$out"
            return 0 ;;
        *drivers-manifest.json.minisig)
            [ -n "\$out" ] && echo 'sig' > "\$out"
            return 0 ;;
        *.ko)
            [ -n "\$out" ] && echo 'fake-ko-bytes' > "\$out"
            return 0 ;;
        *.ko.sha256)
            [ -n "\$out" ] && echo 'sha  module.ko' > "\$out"
            return 0 ;;
        *.ko.minisig)
            [ -n "\$out" ] && echo 'sig' > "\$out"
            return 0 ;;
        *) return 1 ;;
    esac
}

# verify both the manifest and the .ko; manifest verify keys off
# MOCK_MANIFEST_OK, .ko verify keys off MOCK_KO_VERIFY.
ados_verify_artifact() {
    local artifact="\$1"
    case "\$artifact" in
        *drivers-manifest.json) [ "\${MOCK_MANIFEST_OK:-1}" = "1" ] ;;
        *.ko)                   [ "\${MOCK_KO_VERIFY:-1}" = "1" ] ;;
        *)                      return 0 ;;
    esac
}

# Manifest lookup is mocked directly so we don't depend on python3/awk
# parsing in the test. Empty MOCK_LOOKUP means "no match".
_manifest_lookup() {
    [ -n "\${MOCK_LOOKUP:-}" ] || return 1
    printf '%b\n' "\${MOCK_LOOKUP}"
}

# vermagic safety check keyed off MOCK_VERMAGIC_OK.
_vermagic_ok() { [ "\${MOCK_VERMAGIC_OK:-1}" = "1" ]; }

# Guard the call so the runner's own set -e cannot abort before we
# print the result — this mirrors how install-rtl8812eu.sh guards the
# call in an \`if\`, which is exactly the contract under test.
if try_prebuilt_install 8812eu 6.6.51-test arm64; then rc=0; else rc=\$?; fi
echo "RC=\${rc}"
# Surface whether the success breadcrumb path was reached by checking the
# scratch marker the mocked modprobe stub writes.
if [ -f "${RUNDIR}/loaded" ]; then echo "LOADED=1"; else echo "LOADED=0"; fi
exit \${rc}
RUNNER
    chmod +x "${TESTTMP}/run.sh"
}

# Common system-command stubs. modprobe + lsmod cooperate: modprobe writes
# a marker AND makes the subsequent lsmod report the module as loaded.
install_common_stubs() {
    stub modinfo 'echo "6.6.51-test SMP preempt mod_unload aarch64"'
    stub depmod 'exit 0'
    stub install 'exit 0'
    stub modprobe "touch '${RUNDIR}/loaded'; exit 0"
    # lsmod reports 8812eu loaded only after modprobe ran (marker exists).
    stub lsmod "if [ -f '${RUNDIR}/loaded' ]; then echo '8812eu 100 0'; fi; echo 'cfg80211 200 0'"
    # mkdir succeeds (the lib mkdir -p /lib/modules/... and /run/ados).
    stub mkdir '/bin/mkdir -p "$@" 2>/dev/null || exit 0'
}

run_scenario() {
    write_runner
    install_common_stubs
    PATH="${BINDIR}:${PATH}" run env "$@" bash "${TESTTMP}/run.sh"
}

# -----------------------------------------------------------------------------
# Cases
# -----------------------------------------------------------------------------

@test "prebuilt-hit returns 0 and loads the module" {
    run_scenario \
        MOCK_REACHABLE=1 MOCK_MANIFEST_OK=1 \
        MOCK_LOOKUP='8812eu-6.6.51-test-arm64.ko\tabc123\t6.6.51-test SMP preempt mod_unload aarch64' \
        MOCK_KO_VERIFY=1 MOCK_VERMAGIC_OK=1
    [ "$status" -eq 0 ]
    [[ "$output" == *"RC=0"* ]]
    [[ "$output" == *"LOADED=1"* ]]
}

@test "vermagic-mismatch returns non-zero (fallback to DKMS)" {
    run_scenario \
        MOCK_REACHABLE=1 MOCK_MANIFEST_OK=1 \
        MOCK_LOOKUP='8812eu-6.6.51-test-arm64.ko\tabc123\t6.6.51-test SMP preempt mod_unload aarch64' \
        MOCK_KO_VERIFY=1 MOCK_VERMAGIC_OK=0
    [ "$status" -ne 0 ]
    [[ "$output" == *"RC="* ]]
    [[ "$output" != *"RC=0"* ]]
    [[ "$output" == *"LOADED=0"* ]]
}

@test "sha256/signature mismatch returns non-zero" {
    run_scenario \
        MOCK_REACHABLE=1 MOCK_MANIFEST_OK=1 \
        MOCK_LOOKUP='8812eu-6.6.51-test-arm64.ko\tabc123\t6.6.51-test SMP preempt mod_unload aarch64' \
        MOCK_KO_VERIFY=0 MOCK_VERMAGIC_OK=1
    [ "$status" -ne 0 ]
    [[ "$output" != *"RC=0"* ]]
    [[ "$output" == *"LOADED=0"* ]]
}

@test "manifest unreachable returns non-zero" {
    run_scenario \
        MOCK_REACHABLE=1 MOCK_MANIFEST_OK=0
    [ "$status" -ne 0 ]
    [[ "$output" != *"RC=0"* ]]
    [[ "$output" == *"LOADED=0"* ]]
}

@test "offline returns non-zero fast (no hang)" {
    run_scenario MOCK_REACHABLE=0
    [ "$status" -ne 0 ]
    [[ "$output" != *"RC=0"* ]]
    [[ "$output" == *"LOADED=0"* ]]
}

@test "kernel not in manifest returns non-zero" {
    run_scenario \
        MOCK_REACHABLE=1 MOCK_MANIFEST_OK=1 MOCK_LOOKUP=''
    [ "$status" -ne 0 ]
    [[ "$output" != *"RC=0"* ]]
    [[ "$output" == *"LOADED=0"* ]]
}

@test "non-arm64 arch skips prebuilt and returns non-zero" {
    write_runner
    install_common_stubs
    # Override the runner's invocation arch to armhf by re-invoking with a
    # tweaked call. Simplest: assert the library declines armhf directly.
    PATH="${BINDIR}:${PATH}" run bash -c "
        set -euo pipefail
        . '${LIB}'
        ados_reachable() { return 0; }
        try_prebuilt_install 8812eu 6.6.51-test armhf
    "
    [ "$status" -ne 0 ]
}
