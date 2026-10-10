#!/usr/bin/env bash
# The bootstrap must remove its temp dir (the downloaded installer and its
# sidecars) after the installer exits, on success and on failure, and must pass
# the installer's exit status on unchanged. /tmp is tmpfs (RAM) on most boards,
# so a leaked dir per run is a slow memory leak.
#
# Hermetic: the release tree is a directory served over file://
# (ADOS_RELEASE_BASE), the installer is a stub, and `uname`, `id`, `apt-get`
# and `minisign` are stubs, so the test touches no network and needs no root.
# The temp root is a private TMPDIR, so the leak check counts only this run.
set -eu

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

BIN="${WORK}/bin"
REL="${WORK}/release"
TMPROOT="${WORK}/tmproot"
SEEN="${WORK}/seen"
mkdir -p "$BIN" "${REL}/prebuilt-installer" "$TMPROOT"

cat > "${BIN}/uname" <<'EOF'
#!/bin/sh
case "$1" in
    -m) echo aarch64 ;;
    *) echo Linux ;;
esac
EOF
printf '#!/bin/sh\necho 0\n' > "${BIN}/id"
printf '#!/bin/sh\nexit 0\n' > "${BIN}/apt-get"
# Signature checking is covered by scripts/test/install_verify.bats; here the
# verifier only has to say yes.
printf '#!/bin/sh\nexit 0\n' > "${BIN}/minisign"
chmod +x "${BIN}/uname" "${BIN}/id" "${BIN}/apt-get" "${BIN}/minisign"

# publish <exit code>: a stub installer that records the dir it ran from and
# exits with <exit code>.
publish() {
    dir="${REL}/prebuilt-installer"
    printf '#!/bin/sh\ndirname "$0" > "%s"\nexit %s\n' "$SEEN" "$1" > "${dir}/ados-installer-aarch64"
    chmod +x "${dir}/ados-installer-aarch64"
    ( cd "$dir" && sha256sum ados-installer-aarch64 > ados-installer-aarch64.sha256 )
    printf 'signature\n' > "${dir}/ados-installer-aarch64.minisig"
}

# A file:// URL for a local directory, on Linux and under Git Bash alike.
rel_path="$(cd "$REL" && { pwd -W 2>/dev/null || pwd; })"
case "$rel_path" in
    /*) ADOS_RELEASE_BASE="file://${rel_path}" ;;
    *) ADOS_RELEASE_BASE="file:///${rel_path}" ;;
esac
export ADOS_RELEASE_BASE

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

run_case() {
    want="$1"
    rm -f "$SEEN"
    publish "$want"
    rc=0
    env PATH="${BIN}:${PATH}" TMPDIR="$TMPROOT" sh "${REPO_ROOT}/scripts/install.sh" --profile drone \
        >/dev/null 2>&1 || rc=$?
    [ -s "$SEEN" ] || fail "installer stub did not run (exit ${rc})"
    [ "$rc" -eq "$want" ] || fail "exit status ${rc}, expected ${want}"
    leftover="$(find "$TMPROOT" -mindepth 1 -maxdepth 1 | wc -l | tr -d ' ')"
    [ "$leftover" -eq 0 ] || fail "temp dir leaked after exit ${want}: $(ls -A "$TMPROOT")"
}

run_case 0
run_case 7
echo "PASS: no temp dir leaked; installer exit status passed through"
