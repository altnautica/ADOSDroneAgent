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
# Source clone location for the macOS build-from-source path (kept for --upgrade).
GIT_URL="https://github.com/altnautica/ADOSDroneAgent.git"

# ── macOS: rootless per-user workstation install ─────────────────────────────
# There is no prebuilt Mach-O installer asset for a Mac, so the installer runs
# from source. This bootstrap ensures the Rust toolchain, resolves the source
# tree (a local checkout, else a shallow clone under $HOME/.ados/src), and runs
# `cargo run -p ados-installer` with the workstation profile. No root: the Rust
# installer registers per-user LaunchAgents under $HOME/.ados.
macos_install() {
    if [ "$(id -u)" -eq 0 ]; then
        echo "ERROR: on macOS run WITHOUT sudo — the ADOS workstation installs as" >&2
        echo "       per-user LaunchAgents under \$HOME/.ados." >&2
        return 2
    fi

    # The install builds the service binaries from source; cargo is required.
    if ! command -v cargo >/dev/null 2>&1 && [ -f "$HOME/.cargo/env" ]; then
        # shellcheck disable=SC1091
        . "$HOME/.cargo/env"
    fi
    if ! command -v cargo >/dev/null 2>&1; then
        echo "ERROR: cargo not found. Install the Rust toolchain (https://rustup.rs)" >&2
        echo "       and re-run — the macOS workstation builds its binaries from source." >&2
        return 1
    fi
    if ! command -v git >/dev/null 2>&1; then
        echo "ERROR: git not found. Install the Xcode Command Line Tools" >&2
        echo "       (xcode-select --install) and re-run." >&2
        return 1
    fi

    # Which branch to clone on a fresh checkout (default main).
    branch="main"
    prev=""
    for a in "$@"; do
        [ "$prev" = "--branch" ] && branch="$a"
        prev="$a"
    done

    # Resolve the source tree: a local checkout this script sits inside, else a
    # shallow clone under $HOME/.ados/src.
    repo=""
    script_dir="$(CDPATH='' cd -- "$(dirname -- "$0")" 2>/dev/null && pwd || true)"
    if [ -n "$script_dir" ] && [ -f "$script_dir/../crates/ados-installer/Cargo.toml" ]; then
        repo="$(CDPATH='' cd -- "$script_dir/.." && pwd)"
    fi
    if [ -z "$repo" ]; then
        src="$HOME/.ados/src"
        if [ -d "$src/.git" ]; then
            echo "Updating ADOS source at $src (branch $branch) …"
            git -C "$src" fetch --depth 1 origin "$branch" >/dev/null 2>&1 || true
            git -C "$src" checkout "$branch" >/dev/null 2>&1 || true
            git -C "$src" reset --hard "origin/$branch" >/dev/null 2>&1 \
                || git -C "$src" pull --ff-only >/dev/null 2>&1 || true
        else
            echo "Cloning ADOS source to $src (branch $branch) …"
            mkdir -p "$(dirname -- "$src")"
            git clone --depth 1 --branch "$branch" "$GIT_URL" "$src" \
                || { echo "ERROR: git clone failed" >&2; return 1; }
        fi
        repo="$src"
    fi

    # Default to the workstation profile unless the operator pinned one.
    has_profile=0
    for a in "$@"; do
        [ "$a" = "--profile" ] && has_profile=1
    done

    echo "Running the ADOS installer from source ($repo/crates) …"
    ( cd "$repo/crates" \
        && if [ "$has_profile" -eq 1 ]; then
               ADOS_SOURCE_DIR="$repo" cargo run --quiet -p ados-installer -- "$@"
           else
               ADOS_SOURCE_DIR="$repo" cargo run --quiet -p ados-installer -- --profile workstation "$@"
           fi )
}

if [ "$(uname -s)" = "Darwin" ]; then
    macos_install "$@"
    exit $?
fi

# 1. Root requirement (Linux). Every later step writes under /opt, /etc, and
#    /etc/systemd/system, so a non-root run cannot proceed.
if [ "$(id -u)" -ne 0 ]; then
    echo "ERROR: run with sudo (root required)" >&2
    exit 1
fi

# 2. Map arch to a prebuilt asset name.
arch="$(uname -m)"
case "$arch" in
    aarch64|arm64) asset="ados-installer-aarch64" ;;
    *)
        echo "ERROR: ${arch} Linux is not supported. The Linux agent ships prebuilt" >&2
        echo "       aarch64 binaries only. Use an aarch64 Linux host, or run on macOS" >&2
        echo "       (which builds from source). A non-aarch64 Linux build-from-source" >&2
        echo "       path is not yet supported." >&2
        exit 1
        ;;
esac

# 3. IPv4-resilient fetch. GitHub over IPv6-only DNS stalls on some boards;
#    force -4 when there is no IPv6 default route, else try then retry with -4.
have_ipv6_default() { ip -6 route show default 2>/dev/null | grep -q .; }

fetch() {
    # fetch <url> <dest> [force4]
    # Cache-Control/Pragma no-cache force a revalidation so a CDN cannot hand
    # back a stale bootstrap binary paired with a stale sha on this rolling tag.
    url="$1"; dest="$2"; force4="${3:-}"
    # --continue-at - resumes a partial transfer so a mid-download drop on a
    # flaky link continues from the last byte instead of restarting from zero.
    if command -v curl >/dev/null 2>&1; then
        if [ -n "$force4" ]; then
            curl -fsSL -H 'Cache-Control: no-cache' -H 'Pragma: no-cache' --connect-timeout 10 --max-time 180 --retry 3 --retry-delay 2 --continue-at - -4 "$url" -o "$dest"
        else
            curl -fsSL -H 'Cache-Control: no-cache' -H 'Pragma: no-cache' --connect-timeout 10 --max-time 180 --retry 3 --retry-delay 2 --continue-at - "$url" -o "$dest"
        fi
    elif command -v wget >/dev/null 2>&1; then
        wget --inet4-only --header='Cache-Control: no-cache' --header='Pragma: no-cache' -q -O "$dest" "$url"
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

# A line of feedback for the short pre-binary window; the installer renders its
# own live progress once it execs below.
echo "Fetching ADOS installer…" >&2
get "${REL_BASE}/${asset}" "${tmp}/${asset}"
get "${REL_BASE}/${asset}.sha256" "${tmp}/${asset}.sha256"
# minisig is opt-in: only fetch when verification can actually run (a pubkey is
# provided and minisign is installed). On the default path it is an unused
# download that would surface a misleading 404, so skip it; silence stderr since
# a missing signature is non-fatal by design.
if [ -n "${ADOS_INSTALLER_PUBKEY:-}" ] && command -v minisign >/dev/null 2>&1; then
    get "${REL_BASE}/${asset}.minisig" "${tmp}/${asset}.minisig" 2>/dev/null || true
fi

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
