#!/bin/sh
# =============================================================================
# ADOS Drone Agent — Installer Bootstrap
#
# This thin POSIX-sh bootstrap fetches, verifies, and execs the prebuilt
# `ados-installer` Rust binary, which orchestrates the full install. Rollback
# is by git history (the prior tree, or a pinned older release asset).
#
# Operator one-liner (unchanged):
#   curl -sSL .../scripts/install.sh | sudo bash -s -- --profile drone ...
#
# All user flags are passed through verbatim to the Rust installer.
# =============================================================================
set -eu

REL_BASE="https://github.com/altnautica/ADOSDroneAgent/releases/download/prebuilt-installer"

# 1. Root requirement (Linux). macOS is a dev host — keep minimal parity with
#    the monolith's dev branch and point the developer at cargo.
if [ "$(uname -s)" = "Darwin" ]; then
    echo "macOS dev host — use: cargo run -p ados-installer -- $*" >&2
    exit 0
fi
if [ "$(id -u)" -ne 0 ]; then
    echo "ERROR: run with sudo (root required)" >&2
    exit 1
fi

# 2. Map arch to a prebuilt asset name.
arch="$(uname -m)"
case "$arch" in
    aarch64|arm64) asset="ados-installer-aarch64" ;;
    *)
        echo "ERROR: no prebuilt ados-installer for ${arch}; build from source: cargo build -p ados-installer" >&2
        exit 1
        ;;
esac

# 3. IPv4-resilient fetch. GitHub over IPv6-only DNS stalls on some boards;
#    force -4 when there is no IPv6 default route, else try then retry with -4.
have_ipv6_default() { ip -6 route show default 2>/dev/null | grep -q .; }

fetch() {
    # fetch <url> <dest> [force4]
    url="$1"; dest="$2"; force4="${3:-}"
    if command -v curl >/dev/null 2>&1; then
        if [ -n "$force4" ]; then
            curl -fsSL --connect-timeout 10 --max-time 180 --retry 3 --retry-delay 2 -4 "$url" -o "$dest"
        else
            curl -fsSL --connect-timeout 10 --max-time 180 --retry 3 --retry-delay 2 "$url" -o "$dest"
        fi
    elif command -v wget >/dev/null 2>&1; then
        wget --inet4-only -q -O "$dest" "$url"
    else
        echo "ERROR: neither curl nor wget is available to fetch the installer" >&2
        exit 1
    fi
}

# Pick a fetch strategy: if no IPv6 default route, go -4 from the start.
get() {
    # get <url> <dest>
    if have_ipv6_default; then
        fetch "$1" "$2" "" || fetch "$1" "$2" "-4"
    else
        fetch "$1" "$2" "-4"
    fi
}

# 4. Temp dir, trap-cleaned on exit.
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp" 2>/dev/null || true' EXIT

get "${REL_BASE}/${asset}" "${tmp}/${asset}"
get "${REL_BASE}/${asset}.sha256" "${tmp}/${asset}.sha256"
# minisig is best-effort — failure to fetch must not abort.
get "${REL_BASE}/${asset}.minisig" "${tmp}/${asset}.minisig" || true

# 5. Verify. sha256 is mandatory; abort loudly on mismatch.
( cd "$tmp" && sha256sum -c "${asset}.sha256" ) || {
    echo "ERROR: sha256 verification failed for ${asset} — aborting (no fallback)" >&2
    exit 1
}
# Optional minisign verification when sig + pubkey + tool are all present.
if [ -s "${tmp}/${asset}.minisig" ] && [ -n "${ADOS_INSTALLER_PUBKEY:-}" ] && command -v minisign >/dev/null 2>&1; then
    minisign -V -P "$ADOS_INSTALLER_PUBKEY" -m "${tmp}/${asset}" -x "${tmp}/${asset}.minisig" || {
        echo "ERROR: minisign verification failed for ${asset} — aborting (no fallback)" >&2
        exit 1
    }
fi

# 6. Install + exec, passing all flags through verbatim.
install -m 0755 "${tmp}/${asset}" "${tmp}/ados-installer"
exec "${tmp}/ados-installer" "$@"
