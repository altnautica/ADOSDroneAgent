#!/usr/bin/env bash
# =============================================================================
# ADOS Drone Agent — Installation Script (dispatcher)
# Supports: Raspberry Pi OS (Bookworm), Ubuntu 22.04+, Armbian, macOS (dev)
# Usage: sudo ./install.sh [CODE]                       (install + pair)
#        sudo ./install.sh --upgrade                    (upgrade only)
#        sudo ./install.sh --force                      (full reinstall)
#        sudo ./install.sh --uninstall                  (remove)
#        sudo ./install.sh --profile <drone|ground-station|lite-rs|auto>
#        sudo ./install.sh --display  <auto|waveshare35a|none|...>
#        sudo ./install.sh --branch   <name>            (track a feature branch)
# Idempotent: re-runs skip completed steps. --pair is a fast path (<5s).
#
# This file is a thin dispatcher. All real work lives in install.d/*.sh
# modules sourced below. See install.d/lib.sh for shared helpers and
# constants. Modules are sourced in numeric order so the dependency
# graph is explicit in the directory listing.
# =============================================================================
set -euo pipefail

# Prevent needrestart and debconf from interfering with the install script
# when invoked via `curl -sSL ... | sudo bash -s -- ...`.
#
# Without these settings, an apt-pulled systemd security upgrade triggers
# `needrestart` which immediately restarts ssh.service. That kills the SSH
# pipe carrying this script's stdin to bash, bash sees EOF mid-execution,
# and any file descriptors pip/sed are holding open get closed mid-write.
# Result: 0-byte systemd unit files in /etc/systemd/system/, 0-byte pydantic
# __init__.py in the venv, and a half-broken install.
#
# Fix: tell needrestart to list-only (NEEDRESTART_MODE=l), suspend prompts
# (NEEDRESTART_SUSPEND=1), and force debconf to noninteractive frontend so
# nothing tries to prompt the user. These exports are inherited by every
# apt-get and dpkg invocation in the rest of this script.
export NEEDRESTART_MODE=l
export NEEDRESTART_SUSPEND=1
export DEBIAN_FRONTEND=noninteractive
export DEBCONF_NOWARNINGS=yes

# ─── Module Sourcing ────────────────────────────────────────────────────────
#
# Resolve our own script directory so we can locate install.d/ both when
# the script lives on disk (git clone) and when bash --rcfile / curl-pipe
# delivers it. When BASH_SOURCE[0] is not a real file (curl-pipe), the
# install.d/ tree is not present; in that case the script is being run
# in lite-rs dispatch mode and the dispatch block below either re-execs
# install-lite.sh or refuses to continue.
ADOS_SCRIPT_DIR=""
if [ -f "${BASH_SOURCE[0]:-}" ]; then
    ADOS_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" 2>/dev/null && pwd)" || ADOS_SCRIPT_DIR=""
fi

# Source helpers + constants first. Without lib.sh's path constants the
# rest of the modules cannot resolve INSTALL_DIR / CONFIG_DIR / etc.
if [ -n "${ADOS_SCRIPT_DIR}" ] && [ -f "${ADOS_SCRIPT_DIR}/install.d/lib.sh" ]; then
    # shellcheck source=install.d/lib.sh
    source "${ADOS_SCRIPT_DIR}/install.d/lib.sh"

    # Source modules in explicit numeric order so the dependency graph
    # is obvious from the directory listing. Function-resolution in bash
    # is dynamic so cross-module calls work regardless of source order;
    # we still go numeric so a code reader can trace responsibility.
    for module in 00-detect 01-state 02-deps 03-kernel 04-dkms 04-usb-otg \
                  05-mesh 06-radio 07-systemd 08-plugin 09-config 10-network \
                  11-artifacts 12-output 13-main; do
        module_path="${ADOS_SCRIPT_DIR}/install.d/${module}.sh"
        if [ ! -f "${module_path}" ]; then
            echo "ERROR: missing install.d module: ${module_path}" >&2
            exit 1
        fi
        # shellcheck source=/dev/null
        source "${module_path}"
    done
    unset module module_path
fi

# ─── Bootstrap dir cleanup trap ─────────────────────────────────────────────
#
# When install.sh was reached via the curl-pipe bootstrap below, the outer
# bootstrap exec()ed us with ADOS_BOOTSTRAP_DIR pointing at the mktemp dir
# that holds the cloned repo. exec() does NOT fire the outer script's
# traps, so the cleanup has to live here in the new process image. Without
# this, every curl-pipe install leaves a ~66 MB tree in /tmp; on the Pi 4B
# (453 MB tmpfs) a handful of installs fills the disk and DKMS extraction
# silently fails partway through.
if [ -n "${ADOS_BOOTSTRAP_DIR:-}" ] && [ -d "${ADOS_BOOTSTRAP_DIR}" ]; then
    # shellcheck disable=SC2064
    trap "rm -rf \"${ADOS_BOOTSTRAP_DIR}\" 2>/dev/null || true" EXIT
fi

# ─── curl-pipe bootstrap ────────────────────────────────────────────────────
#
# When the operator runs `curl -sSL .../install.sh | sudo bash -s -- ...`,
# BASH_SOURCE[0] is not a real file so ADOS_SCRIPT_DIR is empty and the
# install.d/*.sh modules were never sourced above. Without those modules,
# detect_profile, error, do_uninstall, install_system_deps, ... are all
# undefined and the next command would die with "command not found".
#
# Bootstrap by git-cloning the repo to a temp dir and re-execing this
# script from there. The re-execed script sees ADOS_SCRIPT_DIR as a real
# path, sources install.d/*.sh, and proceeds normally. Must run BEFORE
# the lite-rs pre-dispatch (which calls detect_profile from 00-detect.sh).
if [ -z "${ADOS_SCRIPT_DIR}" ]; then
    if ! command -v git >/dev/null 2>&1; then
        # Fresh Radxa BSP images (rsdk-b2 and friends) ship without git, so
        # the canonical curl-pipe one-liner dies at the first command unless
        # the script self-installs git. apt-get install on a system that
        # already has git is a no-op (<1s), so this is safe to always try.
        if command -v apt-get >/dev/null 2>&1; then
            echo "[bootstrap] git not found, installing via apt-get..."
            DEBIAN_FRONTEND=noninteractive apt-get update -qq || true
            DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends git || true
        fi
        if ! command -v git >/dev/null 2>&1; then
            echo "ERROR: git is required to bootstrap installer from curl-pipe" >&2
            echo "       sudo apt-get install -y git" >&2
            exit 1
        fi
    fi
    _BOOT_BRANCH="main"
    # Peek at --branch for the clone so a feature-branch install bootstraps
    # the right tree; the full arg parser below re-reads it.
    _peek_args=("$@")
    for ((_i=0; _i<${#_peek_args[@]}; _i++)); do
        if [ "${_peek_args[$_i]}" = "--branch" ] && [ $((_i+1)) -lt ${#_peek_args[@]} ]; then
            _BOOT_BRANCH="${_peek_args[$((_i+1))]}"
            break
        fi
    done
    # Sweep stale bootstrap dirs from prior install runs before mktemp.
    # Each run leaves a ~66 MB tree of cloned source + vendored
    # submodules behind. On the Pi 4B (453 MB tmpfs) seven runs fill
    # /tmp completely and the next install fails partway through DKMS
    # with "unable to write file". Match only our own pattern (the
    # marker file we drop on every fresh bootstrap) so we never delete
    # an unrelated user's mktemp dir.
    for _stale in /tmp/tmp.*; do
        if [ -f "${_stale}/.ados_bootstrap" ]; then
            rm -rf "${_stale}" 2>/dev/null || true
        fi
    done
    _BOOT_DIR="$(mktemp -d)"
    : > "${_BOOT_DIR}/.ados_bootstrap"
    _BOOT_REPO="https://github.com/altnautica/ADOSDroneAgent.git"
    echo "Bootstrapping installer from ${_BOOT_REPO} (branch ${_BOOT_BRANCH})..."
    # Bounded retry around the bootstrap clone so a transient network blip
    # on a fresh box does not abort the whole install at the first command.
    # modules are not sourced yet on this path, so the retry is inline.
    _BOOT_OK=false
    for _boot_try in 1 2 3; do
        if git clone --depth 1 --recurse-submodules --shallow-submodules \
                     --quiet --branch "${_BOOT_BRANCH}" \
                     "${_BOOT_REPO}" "${_BOOT_DIR}/repo" 2>&1; then
            _BOOT_OK=true
            break
        fi
        echo "[bootstrap] clone attempt ${_boot_try} failed; retrying in $((_boot_try * 3))s..." >&2
        rm -rf "${_BOOT_DIR}/repo" 2>/dev/null || true
        sleep $((_boot_try * 3))
    done
    if [ "${_BOOT_OK}" != "true" ]; then
        echo "ERROR: failed to clone ${_BOOT_REPO} for bootstrap after 3 attempts" >&2
        rm -rf "${_BOOT_DIR}"
        exit 1
    fi
    # Hand the bootstrap path to the exec'd installer via env. The
    # inner script registers a trap that cleans this dir on exit
    # (success or failure). exec does not fire this script's traps so
    # the cleanup must live in the new process image.
    export ADOS_BOOTSTRAP_DIR="${_BOOT_DIR}"
    exec bash "${_BOOT_DIR}/repo/scripts/install.sh" "$@"
fi

# ─── Lite-rs Profile Pre-dispatch ───────────────────────────────────────────
#
# Profile dispatch with board fingerprint auto-detection.
#
# Every operator types the same command. The script reads the running SBC
# (via /proc/device-tree/model + /proc/cpuinfo + /proc/meminfo) and matches
# against the lite-eligible board manifest (lite-boards.json published
# alongside each lite-agent release). If the board is in the manifest OR
# total RAM is at or below 384 MB (failsafe for unknown low-RAM boards),
# the script exec's install-lite.sh. Otherwise it continues into the
# Python full-agent install body below.
#
# Override priority: --profile lite|full|auto flag > ADOS_PROFILE env var
# > auto-detect. --dry-run prints the detected profile and exits.

LITE_BOARDS_MANIFEST_URL="${ADOS_LITE_BOARDS_URL:-https://github.com/altnautica/ADOSDroneAgent/releases/download/lite-agent-main/lite-boards.json}"
LITE_RAM_FAILSAFE_KB=393216  # 384 MB
export LITE_BOARDS_MANIFEST_URL LITE_RAM_FAILSAFE_KB

# Pre-parse --profile / --dry-run without consuming the rest of "$@"; the
# remaining args still flow to the full flag parser below or to
# install-lite.sh on dispatch.
_PROFILE_OVERRIDE=""
_DRY_RUN=false
_PRESCAN_ARGS=("$@")
_i=0
while [ ${_i} -lt ${#_PRESCAN_ARGS[@]} ]; do
    case "${_PRESCAN_ARGS[${_i}]}" in
        --profile)
            _PROFILE_OVERRIDE="${_PRESCAN_ARGS[$((_i+1))]:-}"
            _i=$((_i+2))
            ;;
        --dry-run)
            _DRY_RUN=true
            _i=$((_i+1))
            ;;
        *)
            _i=$((_i+1))
            ;;
    esac
done
export _PROFILE_OVERRIDE

# Resolve the profile.
_PROFILE=""
if [ -n "${_PROFILE_OVERRIDE}" ] && [ "${_PROFILE_OVERRIDE}" != "auto" ]; then
    _PROFILE="${_PROFILE_OVERRIDE}"
elif [ -n "${ADOS_PROFILE:-}" ] && [ "${ADOS_PROFILE}" != "auto" ]; then
    _PROFILE="${ADOS_PROFILE}"
else
    _PROFILE="$(detect_profile)"
fi
# Normalize legacy alias.
[ "${_PROFILE}" = "lite" ] && _PROFILE="lite-rs"

if [ "${_DRY_RUN}" = "true" ]; then
    echo "Detected profile: ${_PROFILE}"
    echo "(run without --dry-run to proceed)"
    exit 0
fi

if [ "${_PROFILE}" = "lite-rs" ]; then
    # Resolve the lite installer. Two modes:
    #   - Local checkout: this script lives on disk; its sibling is too.
    #   - Curl-pipe: this script ran from stdin (BASH_SOURCE[0]="bash"),
    #     and there is no sibling. Fetch from main.
    # We only trust ADOS_SCRIPT_DIR if BASH_SOURCE[0] is a real on-disk file —
    # otherwise dirname resolves to "." and we'd happily exec a random
    # `./install-lite.sh` from the operator's cwd, which is a script
    # injection trap.
    LITE_INSTALLER=""
    if [ -n "${ADOS_SCRIPT_DIR}" ] && [ -x "${ADOS_SCRIPT_DIR}/install-lite.sh" ]; then
        LITE_INSTALLER="${ADOS_SCRIPT_DIR}/install-lite.sh"
    fi
    if [ -z "${LITE_INSTALLER}" ]; then
        LITE_INSTALLER="$(mktemp)"
        # The install-lite.sh script ships as a GitHub Release asset so the
        # bootstrap pin matches whatever release the operator's invocation
        # came from. ADOS_RELEASE_CHANNEL=main resolves to the rolling
        # lite-agent-main pre-release; default and "stable" resolve to the
        # latest stable lite-v* release via the releases/latest redirect.
        case "${ADOS_RELEASE_CHANNEL:-stable}" in
            main)
                LITE_BOOTSTRAP_URL="https://github.com/altnautica/ADOSDroneAgent/releases/download/lite-agent-main/install-lite.sh"
                ;;
            *)
                LITE_BOOTSTRAP_URL="https://github.com/altnautica/ADOSDroneAgent/releases/latest/download/install-lite.sh"
                ;;
        esac
        # Prefer curl, fall back to wget. Buildroot rootfs images ship wget
        # only — the Luckfox Pico Zero is the canonical example.
        if command -v curl >/dev/null 2>&1; then
            curl -fsSL --retry 3 --retry-delay 2 -L \
                "${LITE_BOOTSTRAP_URL}" \
                -o "${LITE_INSTALLER}"
        elif command -v wget >/dev/null 2>&1; then
            wget -q --tries=3 -O "${LITE_INSTALLER}" \
                "${LITE_BOOTSTRAP_URL}"
        else
            echo "ERROR: neither curl nor wget is installed; cannot fetch install-lite.sh" >&2
            exit 1
        fi
        chmod +x "${LITE_INSTALLER}"
    fi
    exec "${LITE_INSTALLER}" "$@"
fi
# else fall through to the Python full-agent install body.

# ─── Full-Agent Install: shared state ───────────────────────────────────────
#
# Modules access these via their var names; export so any subshell-spawning
# helper (e.g. python heredocs) sees the same values. Path constants live
# in install.d/lib.sh; per-invocation state lives here.
BRANCH_NAME=""        # optional feature branch for --branch flag
FRESH_REPO_DIR=""     # temp clone created by the fresh-install path
ADOS_PROFILE="${ADOS_PROFILE:-}"

# ─── Flag Parsing ────────────────────────────────────────────────────────────

PAIR_CODE=""
DRONE_NAME=""
DO_FORCE=false
DO_UPGRADE=false

# Positional pairing code: first non-flag arg that looks like a 4-8 char alphanumeric code
if [ $# -gt 0 ] && [[ "$1" =~ ^[A-Za-z0-9]{4,8}$ ]]; then
    PAIR_CODE="$1"
    shift
fi

while [ $# -gt 0 ]; do
    case "$1" in
        --uninstall)
            do_uninstall
            ;;
        --pair)
            shift
            PAIR_CODE="${1:-}"
            if [ -z "$PAIR_CODE" ]; then
                error "--pair requires a CODE argument"
                exit 1
            fi
            shift
            ;;
        --name)
            shift
            DRONE_NAME="${1:-}"
            if [ -z "$DRONE_NAME" ]; then
                error "--name requires a NAME argument"
                exit 1
            fi
            shift
            ;;
        --force)
            DO_FORCE=true
            shift
            ;;
        --upgrade)
            DO_UPGRADE=true
            shift
            ;;
        --branch)
            # install from a feature branch instead of main
            shift
            BRANCH_NAME="${1:-}"
            if [ -z "$BRANCH_NAME" ]; then
                error "--branch requires a NAME argument"
                exit 1
            fi
            shift
            ;;
        --profile)
            # Already consumed by the pre-dispatch parser at the top of
            # the script. Skip the value too so we don't loop forever.
            shift
            shift 2>/dev/null || true
            ;;
        --display)
            # Operator pins a specific LCD overlay ID (e.g. waveshare35a)
            # for the in-script display provisioner. Equivalent to
            # exporting ADOS_DISPLAY before invoking the script. "auto"
            # (default) lets install_display_driver pick from the board
            # YAML displays.supported list. "none" disables the panel.
            shift
            export ADOS_DISPLAY="${1:-auto}"
            if [ -z "${ADOS_DISPLAY}" ]; then
                error "--display requires a value (e.g. waveshare35a, auto, none)"
                exit 1
            fi
            shift
            ;;
        --dry-run)
            # Same — consumed up top.
            shift
            ;;
        *)
            warn "Unknown option: $1"
            shift
            ;;
    esac
done

# Export shared state so sourced module functions see the same values.
# DRONE_NAME, PAIR_CODE, etc. are read by generate_default_config + the
# main flow; the install.d/ modules expect the var names below.
export ADOS_PROFILE BRANCH_NAME PAIR_CODE DRONE_NAME DO_FORCE DO_UPGRADE FRESH_REPO_DIR

# ─── Main Install Flow ──────────────────────────────────────────────────────
#
# The actual install + upgrade logic lives in install.d/13-main.sh as a
# function. By the time we reach here, all modules are sourced, args are
# parsed, shared state is exported, and the function inherits everything
# via shell scoping. The exit code of main_install_flow becomes our exit.
main_install_flow
