#!/usr/bin/env bats
# =============================================================================
# Bats suite for `scripts/install.sh --ref <commit|branch|tag>`.
#
# Why the flag exists: a deploy that gates on one commit's CI and then installs
# whatever `main` has become can ship a wheel newer than the binaries it gated
# on. That happened — two rigs reported one version while carrying binaries
# from an earlier one, with a fix present in the wheel and absent from the
# binary. `--ref` lets a deploy name the revision it verified.
#
# The invariants under test:
#   1. No --ref            -> byte-for-byte today's behaviour (clone $branch).
#   2. --ref <full SHA>    -> the tree is at exactly that commit.
#   3. --ref <abbrev SHA>  -> resolved, not rejected. A server cannot answer a
#                             request for an abbreviated object name, so the
#                             short form must be expanded rather than read as
#                             "does not exist".
#   4. --ref <unknown>     -> LOUD failure, no fallback to main. Silent
#                             fallback is the entire bug class being closed.
#   5. --ref with no value -> refused before any network work.
#   6. --ref stripped from the argv handed to the Rust installer, which errors
#                             on an unknown flag.
#   7. Linux + --ref       -> FORWARDED to the Rust installer, which checks the
#                             agent tree out at that revision and fetches the
#                             service binaries from that commit's `rev-<sha>`
#                             release. Both halves are now commit-addressable, so
#                             the flag no longer has to be refused here.
#   8. Linux + --ref, over a fake release tree served from disk -> the pinned
#                             asset really resolves at
#                             $ADOS_RELEASE_BASE/rev-<sha>/<asset>, and a
#                             revision with no release is NOT silently served
#                             another one.
#   9. --channel stable --ref -> refused. Stable resolves a wheel from a
#                             v<X.Y.Z> tag, which addresses a version and not a
#                             commit, so there is no per-revision wheel to pin.
#
# Hermetic: the clone source is a local fixture repo (ADOS_GIT_URL), the release
# tree is a directory served over file:// (ADOS_RELEASE_BASE), `cargo` and the
# downloaded installer are stubs that record their argv, and `uname` / `id` /
# `apt-get` are stubbed so the suite touches no network, installs no packages, and
# runs unprivileged on any host.
#
# Assertions go through the helpers below rather than `[[ ... ]]` or `! cmd`.
# Under bats 1.13 both of those are exempt from errexit unless they are the
# LAST command in the test, so a mid-test `[[ "$output" == *"x"* ]]` passes no
# matter what `$output` holds. Every assertion here was verified to fail when
# the behaviour it describes is removed.
# =============================================================================

# Fail the test unless $1 contains $2. A function call is a simple command, so
# errexit fires on a non-zero return wherever it appears in the body.
assert_contains() {
    case "$1" in
        *"$2"*) return 0 ;;
    esac
    printf 'expected output to contain: %s\n--- actual ---\n%s\n' "$2" "$1" >&2
    return 1
}

# Fail the test if $1 contains $2.
assert_not_contains() {
    case "$1" in
        *"$2"*)
            printf 'expected output NOT to contain: %s\n--- actual ---\n%s\n' "$2" "$1" >&2
            return 1
            ;;
    esac
    return 0
}

setup() {
    REPO_ROOT="$(cd "$(dirname "${BATS_TEST_FILENAME}")/../.." && pwd)"
    TMP="$(mktemp -d)"

    # install.sh is copied OUT of the repo on purpose: run from inside a
    # checkout it builds that checkout, which is a different code path.
    RUN="${TMP}/run"
    mkdir -p "${RUN}"
    cp "${REPO_ROOT}/scripts/install.sh" "${RUN}/install.sh"

    # A stub toolchain. `cargo` records its argv so a test can assert the flag
    # was consumed here and never forwarded.
    BIN="${TMP}/bin"
    mkdir -p "${BIN}"
    CARGO_ARGV="${TMP}/cargo.argv"
    cat > "${BIN}/cargo" <<EOF
#!/bin/sh
printf '%s\n' "\$*" > "${CARGO_ARGV}"
exit 0
EOF
    chmod +x "${BIN}/cargo"

    # Platform selector. Every test picks one explicitly. The Linux stub answers
    # `-m` too, because the Linux branch maps the machine type to a prebuilt
    # asset name before it fetches anything.
    darwin_uname() { printf '#!/bin/sh\necho Darwin\n' > "${BIN}/uname"; chmod +x "${BIN}/uname"; }
    linux_uname() {
        cat > "${BIN}/uname" <<'EOF'
#!/bin/sh
case "$1" in
    -m) echo aarch64 ;;
    *) echo Linux ;;
esac
EOF
        chmod +x "${BIN}/uname"
    }

    # The Linux branch writes under /opt and /etc, so it requires root. Stubbed
    # rather than granted: nothing in these tests reaches a real path.
    root_id() { printf '#!/bin/sh\necho 0\n' > "${BIN}/id"; chmod +x "${BIN}/id"; }

    # The bootstrap best-effort installs minisign before verifying. A no-op stub
    # keeps the suite from touching a package manager on any host; the fetch then
    # reports its sha256-only posture and continues, exactly as on a board with
    # no package index.
    stub_apt() { printf '#!/bin/sh\nexit 0\n' > "${BIN}/apt-get"; chmod +x "${BIN}/apt-get"; }

    # A fake release tree the bootstrap and the installer both fetch from over
    # file://, which is the whole point of ADOS_RELEASE_BASE: `net.rs` and this
    # script shell out to curl, and curl speaks file://, so a directory on disk
    # exercises the real fetch path with no network and no credential.
    #
    # It carries two things: the installer asset the bootstrap verifies and execs
    # (a stub that records its argv), and one prebuilt under the per-revision tag
    # `rev-<TIP_SHA>`, laid out exactly as `fetch_binaries::asset_base` builds it.
    publish_release_tree() {
        REL="${TMP}/release"
        INSTALLER_ARGV="${TMP}/installer.argv"
        REV_FETCHED="${TMP}/rev-fetched.bin"
        mkdir -p "${REL}/prebuilt-installer" "${REL}/rev-${TIP_SHA}"
        cat > "${REL}/prebuilt-installer/ados-installer-aarch64" <<EOF
#!/bin/sh
# Stand-in for the Rust installer: record the argv the bootstrap handed over,
# then resolve one prebuilt the way fetch_binaries does under a pin
# (asset_base(rev, tag) == \${ADOS_RELEASE_BASE}/rev-<sha>).
printf '%s\n' "\$*" > "${INSTALLER_ARGV}"
rev=""
prev=""
for a in "\$@"; do
    [ "\$prev" = "--ref" ] && rev="\$a"
    prev="\$a"
done
[ -n "\$rev" ] || exit 0
curl -fsSL "\${ADOS_RELEASE_BASE}/rev-\${rev}/ados-supervisor-aarch64" -o "${REV_FETCHED}" || exit 9
EOF
        chmod +x "${REL}/prebuilt-installer/ados-installer-aarch64"
        ( cd "${REL}/prebuilt-installer" \
            && sha256sum ados-installer-aarch64 > ados-installer-aarch64.sha256 )
        printf 'pinned-supervisor-bytes\n' > "${REL}/rev-${TIP_SHA}/ados-supervisor-aarch64"
        ( cd "${REL}/rev-${TIP_SHA}" \
            && sha256sum ados-supervisor-aarch64 > ados-supervisor-aarch64.sha256 )
        export ADOS_RELEASE_BASE="file://${REL}"
    }

    # The fixture "remote": two commits on main. allowAnySHA1InWant mirrors
    # github.com, where fetching a bare object name is what makes a SHA pin
    # possible at all.
    FIXTURE="${TMP}/fixture"
    mkdir -p "${FIXTURE}"
    git init -q -b main "${FIXTURE}"
    git -C "${FIXTURE}" config user.email t@example.com
    git -C "${FIXTURE}" config user.name t
    git -C "${FIXTURE}" config uploadpack.allowAnySHA1InWant true
    mkdir -p "${FIXTURE}/crates/ados-installer"
    echo old > "${FIXTURE}/marker"
    touch "${FIXTURE}/crates/ados-installer/Cargo.toml"
    git -C "${FIXTURE}" add -A
    git -C "${FIXTURE}" commit -qm first
    OLD_SHA="$(git -C "${FIXTURE}" rev-parse HEAD)"
    echo new > "${FIXTURE}/marker"
    git -C "${FIXTURE}" add -A
    git -C "${FIXTURE}" commit -qm second
    TIP_SHA="$(git -C "${FIXTURE}" rev-parse HEAD)"

    HOME_DIR="${TMP}/home"
    mkdir -p "${HOME_DIR}"
    SRC="${HOME_DIR}/.ados/src"

    export ADOS_GIT_URL="file://${FIXTURE}"
}

teardown() {
    rm -rf "${TMP}"
}

# Run the bootstrap with the stub toolchain in front of PATH.
boot() {
    run env HOME="${HOME_DIR}" PATH="${BIN}:${PATH}" ADOS_GIT_URL="${ADOS_GIT_URL}" \
        sh "${RUN}/install.sh" "$@"
}

@test "no --ref: clones the default branch, exactly as before" {
    darwin_uname
    boot --profile workstation
    [ "$status" -eq 0 ]
    [ "$(git -C "${SRC}" rev-parse HEAD)" = "${TIP_SHA}" ]
    # argv reaches the installer untouched.
    grep -q -- '--profile workstation' "${CARGO_ARGV}"
}

@test "no --ref: a second run still follows the branch forward" {
    # The unpinned path has two halves — clone, then update an existing
    # checkout. Only the clone half is covered above, and "absent --ref behaves
    # exactly as today" is the requirement most expensive to get wrong.
    darwin_uname
    boot --profile workstation
    [ "$status" -eq 0 ]
    [ "$(git -C "${SRC}" rev-parse HEAD)" = "${TIP_SHA}" ]

    echo newer > "${FIXTURE}/marker"
    git -C "${FIXTURE}" add -A
    git -C "${FIXTURE}" commit -qm third
    newtip="$(git -C "${FIXTURE}" rev-parse HEAD)"

    boot --profile workstation
    [ "$status" -eq 0 ]
    [ "$(git -C "${SRC}" rev-parse HEAD)" = "${newtip}" ]
}

@test "--ref <full SHA>: the tree is at that exact commit" {
    darwin_uname
    boot --profile workstation --ref "${OLD_SHA}"
    [ "$status" -eq 0 ]
    [ "$(git -C "${SRC}" rev-parse HEAD)" = "${OLD_SHA}" ]
    [ "$(cat "${SRC}/marker")" = "old" ]
}

@test "--ref <abbreviated SHA>: resolved, not rejected" {
    darwin_uname
    short="$(printf '%s' "${OLD_SHA}" | cut -c1-8)"
    boot --profile workstation --ref "${short}"
    [ "$status" -eq 0 ]
    [ "$(git -C "${SRC}" rev-parse HEAD)" = "${OLD_SHA}" ]
}

@test "--ref is consumed here, never forwarded to the Rust installer" {
    # The installer rejects an unknown flag, so a forwarded --ref would abort
    # every pinned install.
    darwin_uname
    boot --profile workstation --ref "${OLD_SHA}" --upgrade
    [ "$status" -eq 0 ]
    assert_not_contains "$(cat "${CARGO_ARGV}")" "--ref"
    grep -q -- '--profile workstation' "${CARGO_ARGV}"
    grep -q -- '--upgrade' "${CARGO_ARGV}"
}

@test "--ref <unknown>: fails loudly and does not fall back to main" {
    darwin_uname
    boot --profile workstation --ref deadbeefdeadbeefdeadbeefdeadbeefdeadbeef
    [ "$status" -ne 0 ]
    assert_contains "$output" "Not falling back to main"
    # Nothing was built, and no stale tree was passed off as the pin.
    [ ! -f "${CARGO_ARGV}" ]
    run git -C "${SRC}" rev-parse HEAD
    [ "$status" -ne 0 ]
}

@test "--ref <unknown> does not silently reuse an existing checkout" {
    # The dangerous shape: a good tree already on disk, then a bad pin. It must
    # not build the tree that happens to be there.
    darwin_uname
    boot --profile workstation --ref "${TIP_SHA}"
    [ "$status" -eq 0 ]
    rm -f "${CARGO_ARGV}"
    boot --profile workstation --ref ffffffffffffffffffffffffffffffffffffffff
    [ "$status" -ne 0 ]
    [ ! -f "${CARGO_ARGV}" ]
}

@test "--ref with no value is refused before any work" {
    darwin_uname
    boot --profile workstation --ref
    [ "$status" -eq 2 ]
    assert_contains "$output" "--ref expects a value"
    [ ! -d "${SRC}" ]
}

@test "--ref followed by another flag is refused" {
    darwin_uname
    boot --ref --profile workstation
    [ "$status" -eq 2 ]
    assert_contains "$output" "--ref expects a value"
}

@test "re-pinning an existing checkout moves it to the new revision" {
    darwin_uname
    boot --profile workstation --ref "${TIP_SHA}"
    [ "$status" -eq 0 ]
    [ "$(git -C "${SRC}" rev-parse HEAD)" = "${TIP_SHA}" ]
    boot --profile workstation --ref "${OLD_SHA}"
    [ "$status" -eq 0 ]
    [ "$(git -C "${SRC}" rev-parse HEAD)" = "${OLD_SHA}" ]
}

@test "Linux forwards --ref instead of refusing it" {
    # The inverse of the old invariant. It used to be refused because nothing the
    # Linux path fetched was addressable by commit: the binaries came from rolling
    # per-service tags and the wheel from a version tag. CI now publishes a
    # per-revision release and the installer resolves against it, so the flag is
    # forwarded rather than rejected, and BOTH halves carry the same commit.
    linux_uname
    root_id
    stub_apt
    publish_release_tree
    boot --profile drone --upgrade --ref "${TIP_SHA}"
    [ "$status" -eq 0 ]
    assert_not_contains "$output" "not supported on Linux"
    assert_contains "$(cat "${INSTALLER_ARGV}")" "--ref ${TIP_SHA}"
    grep -q -- '--profile drone' "${INSTALLER_ARGV}"
    grep -q -- '--upgrade' "${INSTALLER_ARGV}"
}

@test "a pinned Linux install resolves its binaries from that revision's release" {
    # End to end over file://: the bootstrap verifies and execs the installer
    # asset out of the fake release tree, exports the release base, and the
    # installer resolves rev-<sha>/<asset> from it. This is the half the old
    # refusal said could not exist.
    linux_uname
    root_id
    stub_apt
    publish_release_tree
    boot --profile drone --ref "${TIP_SHA}"
    [ "$status" -eq 0 ]
    [ "$(cat "${REV_FETCHED}")" = "pinned-supervisor-bytes" ]
}

@test "a revision with no published release is not silently served another one" {
    # Only rev-<TIP_SHA> exists in the tree. Pinning the other commit must fail
    # rather than resolve to whatever release happens to be there — a pin that
    # falls back is the entire bug class this flag closes, and on the Linux path
    # the binaries are where it would fall back silently.
    linux_uname
    root_id
    stub_apt
    publish_release_tree
    boot --profile drone --ref "${OLD_SHA}"
    [ "$status" -ne 0 ]
    [ ! -f "${REV_FETCHED}" ]
}

@test "--channel stable with --ref is refused before anything is fetched" {
    # Stable resolves its wheel from a v<X.Y.Z> tag, which addresses a version
    # and not a commit, so there is no per-revision wheel to pin. Honouring only
    # the binary half would report a pin the wheel does not have.
    linux_uname
    root_id
    stub_apt
    publish_release_tree
    boot --profile drone --channel stable --ref "${TIP_SHA}"
    [ "$status" -eq 2 ]
    assert_contains "$output" "--ref requires --channel edge"
    # Refused before the installer was even downloaded.
    [ ! -f "${INSTALLER_ARGV}" ]
}

@test "Linux without --ref is untouched" {
    linux_uname
    boot --profile drone --upgrade
    assert_not_contains "$output" "--ref"
    # Unprivileged, so it stops at the existing root check — unchanged.
    assert_contains "$output" "root required"
}
