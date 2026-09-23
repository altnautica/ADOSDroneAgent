#!/usr/bin/env bats
# =============================================================================
# Bats suite for the bootstrap's verification of the installer it execs as root.
#
# The invariants under test:
#   1. A correctly signed installer is verified and run.
#   2. No .minisig published            -> refused, installer never runs.
#   3. A signature from another key     -> refused.
#   4. A swapped binary with a matching -> refused: the .sha256 comes from the
#      .sha256 but the old signature       same host, so only the signature
#                                          proves origin.
#   5. No minisign on the host          -> refused, never a sha256-only run.
#   6. --channel stable --version X     -> the installer comes from the v<X>
#                                          release, not the rolling tag.
#   7. --channel stable, no --version   -> refused before any download.
#
# Hermetic in the same way as install_ref_pin.bats: the release tree is a
# directory served over file:// (ADOS_RELEASE_BASE), the installer is a stub
# that records it ran, and `uname` / `id` / `apt-get` are stubbed. Requires a
# real `minisign` on PATH (installed by the CI job).
#
# Assertions go through helper functions rather than `[[ ... ]]` or `! cmd`,
# which bats exempts from errexit unless they are the last command in a test.
# =============================================================================

assert_contains() {
    case "$1" in
        *"$2"*) return 0 ;;
    esac
    printf 'expected output to contain: %s\n--- actual ---\n%s\n' "$2" "$1" >&2
    return 1
}

setup() {
    REPO_ROOT="$(cd "$(dirname "${BATS_TEST_FILENAME}")/../.." && pwd)"
    TMP="$(mktemp -d)"
    RUN="${TMP}/run"
    mkdir -p "${RUN}"
    cp "${REPO_ROOT}/scripts/install.sh" "${RUN}/install.sh"

    BIN="${TMP}/bin"
    mkdir -p "${BIN}"
    cat > "${BIN}/uname" <<'EOF'
#!/bin/sh
case "$1" in
    -m) echo aarch64 ;;
    *) echo Linux ;;
esac
EOF
    printf '#!/bin/sh\necho 0\n' > "${BIN}/id"
    printf '#!/bin/sh\nexit 0\n' > "${BIN}/apt-get"
    chmod +x "${BIN}/uname" "${BIN}/id" "${BIN}/apt-get"

    # The trusted release key and an unrelated one.
    minisign -G -W -p "${TMP}/release.pub" -s "${TMP}/release.key" >/dev/null
    minisign -G -W -p "${TMP}/other.pub" -s "${TMP}/other.key" >/dev/null
    ADOS_INSTALLER_PUBKEY="$(tail -n 1 "${TMP}/release.pub")"
    export ADOS_INSTALLER_PUBKEY

    REL="${TMP}/release"
    RAN="${TMP}/installer.ran"
    export ADOS_RELEASE_BASE="file://${REL}"
    publish_installer prebuilt-installer rolling
}

teardown() {
    rm -rf "${TMP}"
}

# publish_installer <tag> <marker> [key]
#   Publish a stub installer under <tag> that writes <marker> to $RAN when it
#   runs, with a .sha256 and a .minisig made with [key] (default: release).
publish_installer() {
    dir="${REL}/$1"
    mkdir -p "${dir}"
    printf '#!/bin/sh\necho %s > "%s"\n' "$2" "${RAN}" > "${dir}/ados-installer-aarch64"
    chmod +x "${dir}/ados-installer-aarch64"
    ( cd "${dir}" && sha256sum ados-installer-aarch64 > ados-installer-aarch64.sha256 )
    rm -f "${dir}/ados-installer-aarch64.minisig"
    minisign -S -s "${TMP}/${3:-release}.key" -m "${dir}/ados-installer-aarch64" >/dev/null
}

boot() {
    run env PATH="${BIN}:${PATH}" sh "${RUN}/install.sh" "$@"
}

@test "a correctly signed installer is verified and run" {
    boot --profile drone
    [ "$status" -eq 0 ]
    [ "$(cat "${RAN}")" = "rolling" ]
}

@test "an installer with no published signature is refused" {
    rm -f "${REL}/prebuilt-installer/ados-installer-aarch64.minisig"
    boot --profile drone
    [ "$status" -ne 0 ]
    assert_contains "$output" "no signature published"
    [ ! -f "${RAN}" ]
}

@test "a signature from another key is refused" {
    publish_installer prebuilt-installer rolling other
    boot --profile drone
    [ "$status" -ne 0 ]
    assert_contains "$output" "does not match the release key"
    [ ! -f "${RAN}" ]
}

@test "a swapped binary with a matching sha256 is refused" {
    dir="${REL}/prebuilt-installer"
    printf '#!/bin/sh\necho swapped > "%s"\n' "${RAN}" > "${dir}/ados-installer-aarch64"
    ( cd "${dir}" && sha256sum ados-installer-aarch64 > ados-installer-aarch64.sha256 )
    boot --profile drone
    [ "$status" -ne 0 ]
    assert_contains "$output" "does not match the release key"
    [ ! -f "${RAN}" ]
}

@test "a host without minisign refuses rather than falling back to sha256" {
    # A PATH carrying every tool the bootstrap uses except minisign.
    SHIM="${TMP}/shim"
    mkdir -p "${SHIM}"
    for t in sh mktemp rm curl grep sha256sum dirname basename install sed head cat chmod env tail; do
        p="$(command -v "$t")" && ln -s "$p" "${SHIM}/$t"
    done
    run env PATH="${BIN}:${SHIM}" sh "${RUN}/install.sh" --profile drone
    [ "$status" -ne 0 ]
    assert_contains "$output" "minisign is required"
    [ ! -f "${RAN}" ]
}

@test "--channel stable runs the installer from its own version release" {
    publish_installer v1.2.3 pinned
    boot --profile drone --channel stable --version 1.2.3
    [ "$status" -eq 0 ]
    [ "$(cat "${RAN}")" = "pinned" ]
}

@test "--channel stable is verified like every other channel" {
    publish_installer v1.2.3 pinned
    rm -f "${REL}/v1.2.3/ados-installer-aarch64.minisig"
    boot --profile drone --channel stable --version v1.2.3
    [ "$status" -ne 0 ]
    assert_contains "$output" "no signature published"
    [ ! -f "${RAN}" ]
}

@test "--channel stable without --version is refused before any download" {
    rm -rf "${REL}"
    boot --profile drone --channel stable
    [ "$status" -eq 2 ]
    assert_contains "$output" "requires --version"
}
