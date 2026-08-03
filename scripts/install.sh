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
# All user flags are passed through verbatim to the Rust installer, with one
# exception: `--ref <commit|branch|tag>` is consumed here. It pins the macOS
# build-from-source path to an exact revision, and is refused on Linux, where
# nothing this bootstrap fetches is addressable by commit (see the block guarding
# it below).
# =============================================================================
set -eu

REL_BASE="https://github.com/altnautica/ADOSDroneAgent/releases/download/prebuilt-installer"
# Source clone location for the macOS build-from-source path (kept for --upgrade).
# Overridable so the shell suite can point the clone at a local fixture repo;
# an operator never sets it.
GIT_URL="${ADOS_GIT_URL:-https://github.com/altnautica/ADOSDroneAgent.git}"

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

    # Source selectors, read before any work so a malformed pin fails before the
    # toolchain probes rather than after them:
    #   --branch <name>  branch to clone on a fresh checkout (default main)
    #   --ref <rev>      pin to an exact revision — commit SHA, branch, or tag
    # --ref wins over --branch when both are given (it is the stricter of the
    # two). It is consumed here and stripped below, because the Rust installer
    # rejects flags it does not know.
    branch="main"
    ref=""
    saw_ref=0
    prev=""
    for a in "$@"; do
        [ "$prev" = "--branch" ] && branch="$a"
        [ "$prev" = "--ref" ] && ref="$a"
        [ "$a" = "--ref" ] && saw_ref=1
        prev="$a"
    done
    if [ "$saw_ref" -eq 1 ]; then
        # A --ref with no value would otherwise read as "no pin" and install
        # latest main under the impression it had pinned something.
        case "$ref" in
            '' | -*)
                echo "ERROR: --ref expects a value: a commit SHA, branch, or tag." >&2
                echo "       Example: --ref 3b4b8dee" >&2
                return 2
                ;;
        esac
        # Strip `--ref <rev>` from the argv forwarded to the Rust installer,
        # which errors on an unknown flag. Rotate through the positional
        # parameters so values containing spaces survive.
        argc=$#
        n=0
        skip=0
        while [ "$n" -lt "$argc" ]; do
            a="$1"
            shift
            n=$((n + 1))
            if [ "$skip" -eq 1 ]; then
                skip=0
                continue
            fi
            if [ "$a" = "--ref" ]; then
                skip=1
                continue
            fi
            set -- "$@" "$a"
        done
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

    # Resolve the source tree: a local checkout this script sits inside, else a
    # shallow clone under $HOME/.ados/src.
    repo=""
    script_dir="$(CDPATH='' cd -- "$(dirname -- "$0")" 2>/dev/null && pwd || true)"
    if [ -n "$script_dir" ] && [ -f "$script_dir/../crates/ados-installer/Cargo.toml" ]; then
        repo="$(CDPATH='' cd -- "$script_dir/.." && pwd)"
    fi
    if [ -n "$repo" ] && [ -n "$ref" ]; then
        # Running from a checkout the operator controls. Moving its HEAD would
        # discard whatever they have in it, and building it as-is would ignore
        # the pin while reporting success — so neither. They pin it themselves.
        echo "ERROR: --ref cannot be applied to the local checkout at $repo." >&2
        echo "       That tree is yours; the installer will not move its HEAD." >&2
        echo "       Check the revision out yourself and re-run without --ref:" >&2
        echo "         git -C $repo checkout $ref" >&2
        return 2
    fi
    if [ -z "$repo" ]; then
        src="$HOME/.ados/src"
        if [ -n "$ref" ]; then
            # An exact-revision pin. `git clone --branch` takes a branch or a
            # tag but never a commit SHA, so the pinned path is always
            # fetch + checkout FETCH_HEAD, over a checkout created here if
            # there is not one already.
            #
            # Every step is checked. The unpinned path below tolerates a failed
            # fetch (`|| true`) and builds whatever is on disk; doing that with
            # a pin would silently install a different revision than the one
            # asked for, which is the entire failure this flag exists to stop.
            if [ -d "$src/.git" ]; then
                echo "Updating ADOS source at $src (ref $ref) …"
                git -C "$src" remote set-url origin "$GIT_URL" 2>/dev/null || true
            else
                echo "Creating ADOS source checkout at $src (ref $ref) …"
                mkdir -p "$src"
                git init -q "$src" \
                    || { echo "ERROR: git init $src failed" >&2; return 1; }
                git -C "$src" remote add origin "$GIT_URL" 2>/dev/null \
                    || git -C "$src" remote set-url origin "$GIT_URL" \
                    || { echo "ERROR: could not set the origin remote" >&2; return 1; }
            fi
            pinned=""
            if git -C "$src" fetch --depth 1 origin "$ref"; then
                pinned="FETCH_HEAD"
            else
                # A server cannot resolve an ABBREVIATED commit: the fetch asks
                # for an exact object name, so `3b4b8dee` is refused with the
                # same "couldn't find remote ref" a typo gets. Treating that as
                # "does not exist" would reject a perfectly good pin, so deepen
                # the branch history once and resolve the prefix locally.
                # Bounded on purpose — a commit not in the last 500 of $branch
                # is reported, not chased.
                #
                # The guard below takes hex refs SHORTER than a full object
                # name. A 40-char SHA the server already refused is genuinely
                # absent, so it fails fast rather than paying for a deep fetch
                # that cannot contain it.
                case "$ref" in
                    *[!0-9a-fA-F]* | '') : ;;
                    ????????????????????????????????????????*) : ;;
                    *)
                        echo "'$ref' is an abbreviated commit; deepening $branch to resolve it …"
                        git -C "$src" fetch --depth 500 origin "$branch" >/dev/null 2>&1 || true
                        full="$(git -C "$src" rev-parse --verify --quiet "${ref}^{commit}" || true)"
                        [ -n "$full" ] && pinned="$full"
                        ;;
                esac
            fi
            if [ -z "$pinned" ]; then
                echo "ERROR: could not resolve '$ref' in $GIT_URL." >&2
                echo "       Check that the commit, branch, or tag exists and is pushed." >&2
                echo "       An abbreviated commit is resolvable only if it is within the" >&2
                echo "       last 500 commits of '$branch'; otherwise pass the full" >&2
                echo "       40-character SHA." >&2
                echo "       Not falling back to main — nothing was installed." >&2
                return 1
            fi
            if ! git -C "$src" checkout -q --force --detach "$pinned"; then
                echo "ERROR: resolved '$ref' but could not check it out at $src." >&2
                return 1
            fi
            echo "Source pinned to $(git -C "$src" rev-parse HEAD)"
        elif [ -d "$src/.git" ]; then
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

# 0. --ref is a macOS-only flag, and saying so is the point of this block.
#
# The macOS path builds every binary from the tree it checks out, so pinning
# that tree pins the install. The Linux path cannot make the same promise: this
# bootstrap fetches only the installer binary, and the installer resolves the
# service binaries from ROLLING per-service release tags (`prebuilt-supervisor`,
# `prebuilt-video`, … — see crates/ados-installer/src/binaries.rs) and the wheel
# from a version tag or a branch clone (crates/ados-installer/src/steps/
# venv_agent.rs). Neither is addressable by commit, and no release carries both
# for one revision, so there is nothing here a SHA could select.
#
# Accepting the flag and pinning only what happens to be pinnable is worse than
# refusing it: a deploy would report a pin it does not have, which is how a rig
# ends up carrying a wheel from one revision and binaries from another. Making
# this real needs a per-revision release in CI plus an installer that resolves
# against it — an installer change, not a bootstrap change.
for a in "$@"; do
    if [ "$a" = "--ref" ]; then
        echo "ERROR: --ref is not supported on Linux." >&2
        echo "       The Linux install fetches prebuilt binaries from rolling" >&2
        echo "       per-service release tags and the wheel from a version tag," >&2
        echo "       so no commit can be selected here. Honoring --ref would pin" >&2
        echo "       nothing while reporting that it had." >&2
        echo "       To pin the agent package, use: --channel edge --branch <branch-or-tag>" >&2
        echo "       (that pins the wheel only; the Rust binaries stay rolling)." >&2
        exit 2
    fi
done

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

# Trust anchor for the prebuilt installer binary: the vendored public half of
# the ADOS_DRIVER_SIGNING_KEY minisign keypair. An operator may override it with
# their own key. When a signed .minisig exists (CI signs once the secret is set)
# it is verified below; until then the mandatory sha256 is the only gate.
: "${ADOS_INSTALLER_PUBKEY:=RWQ/CJ1+gk7rjVfGSoy6MOL50e8TmO30KD/J+goaEj+WMI1uzEf92rHN}"

# The bootstrap carries its own verifier.
#
# Signature checking below is conditional on minisign being present, and a
# stock board image does not ship it — so on the path that matters, a fresh
# install, verification silently did not happen and the only gate was sha256
# (which a compromised release host controls just as easily as the binary).
# Install it before the check rather than skipping the check. Best-effort: a
# box with no package manager or no network still installs, still enforces
# sha256, and says which posture it is in.
if ! command -v minisign >/dev/null 2>&1; then
    if command -v apt-get >/dev/null 2>&1; then
        echo "Installing minisign to verify the download…" >&2
        # `update` first. A freshly flashed image ships an emptied package
        # index to save space, so the install below fails with "unable to
        # locate package" on exactly the path this matters most — a fresh
        # install — and the whole verification step silently does nothing.
        DEBIAN_FRONTEND=noninteractive apt-get update -qq >/dev/null 2>&1 || true
        if ! DEBIAN_FRONTEND=noninteractive apt-get install -y -qq minisign >/dev/null 2>&1; then
            # Say why, rather than leaving the operator to infer it from the
            # posture line below.
            echo "WARNING: could not install minisign from apt." >&2
        fi
    else
        echo "WARNING: no apt-get; cannot install a signature verifier." >&2
    fi
fi
if command -v minisign >/dev/null 2>&1; then
    echo "Signature verification: enabled." >&2
else
    echo "Signature verification: unavailable (minisign not installed); sha256 only." >&2
fi

# A line of feedback for the short pre-binary window; the installer renders its
# own live progress once it execs below.
echo "Fetching ADOS installer…" >&2
get "${REL_BASE}/${asset}" "${tmp}/${asset}"
get "${REL_BASE}/${asset}.sha256" "${tmp}/${asset}.sha256"
# Fetch the signature whenever verification can run. Every artifact in the
# catalog is signed, so a missing one is now worth reporting rather than
# passing over in silence.
if [ -n "${ADOS_INSTALLER_PUBKEY:-}" ] && command -v minisign >/dev/null 2>&1; then
    get "${REL_BASE}/${asset}.minisig" "${tmp}/${asset}.minisig" 2>/dev/null || true
    [ -s "${tmp}/${asset}.minisig" ] || \
        echo "WARNING: no signature published for ${asset}; sha256 only." >&2
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
