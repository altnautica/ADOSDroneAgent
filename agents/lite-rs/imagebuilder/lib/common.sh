#!/usr/bin/env bash
#
# imagebuilder/lib/common.sh — shared helpers used by every board recipe.
#
# Sourced (not executed) by build-driver.sh. Functions live under the
# `imgbuild::` namespace so they don't collide with bash builtins or
# the recipe's own helpers.

# Prevent double-sourcing.
[ -n "${IMGBUILD_COMMON_SH:-}" ] && return 0
IMGBUILD_COMMON_SH=1

# Resolve repo + imagebuilder roots once at source-time so callers
# don't have to thread these around.
IMGBUILD_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_ROOT="$(cd -- "${IMGBUILD_ROOT}/../../.." && pwd)"
IMGBUILD_OUTPUT="${IMGBUILD_OUTPUT:-${REPO_ROOT}/output}"
IMGBUILD_VERSION="${IMGBUILD_VERSION:-0.1.0}"
IMGBUILD_AGENT_RELEASE_TAG="${IMGBUILD_AGENT_RELEASE_TAG:-lite-agent-main}"
IMGBUILD_AGENT_REPO="${IMGBUILD_AGENT_REPO:-altnautica/ADOSDroneAgent}"

export IMGBUILD_ROOT REPO_ROOT IMGBUILD_OUTPUT IMGBUILD_VERSION
export IMGBUILD_AGENT_RELEASE_TAG IMGBUILD_AGENT_REPO

# ---------------------------------------------------------------------
# Logging — colored stderr
# ---------------------------------------------------------------------

if [ -t 2 ]; then
    _RST=$'\e[0m'
    _BOLD=$'\e[1m'
    _RED=$'\e[31m'
    _YEL=$'\e[33m'
    _GRN=$'\e[32m'
    _CYA=$'\e[36m'
else
    _RST="" _BOLD="" _RED="" _YEL="" _GRN="" _CYA=""
fi

imgbuild::log_info()  { printf '%s[imgbuild]%s %s\n'      "${_CYA}" "${_RST}" "$*" >&2; }
imgbuild::log_step()  { printf '%s[imgbuild]%s %s%s%s\n'  "${_CYA}" "${_RST}" "${_BOLD}" "$*" "${_RST}" >&2; }
imgbuild::log_warn()  { printf '%s[imgbuild]%s %sWARN%s %s\n' "${_YEL}" "${_RST}" "${_BOLD}" "${_RST}" "$*" >&2; }
imgbuild::log_error() { printf '%s[imgbuild]%s %sERROR%s %s\n' "${_RED}" "${_RST}" "${_BOLD}" "${_RST}" "$*" >&2; }
imgbuild::log_ok()    { printf '%s[imgbuild]%s %sOK%s %s\n'  "${_GRN}" "${_RST}" "${_BOLD}" "${_RST}" "$*" >&2; }

# ---------------------------------------------------------------------
# Self-check — invoked by `build-driver.sh --check`
# ---------------------------------------------------------------------

imgbuild::check() {
    local fail=0

    imgbuild::log_step "Checking imagebuilder/ scaffolding…"

    [ -d "${IMGBUILD_ROOT}/lib" ]      || { imgbuild::log_error "lib/ missing";      fail=1; }
    [ -d "${IMGBUILD_ROOT}/overlay" ]  || { imgbuild::log_error "overlay/ missing";  fail=1; }
    [ -d "${IMGBUILD_ROOT}/boards" ]   || { imgbuild::log_error "boards/ missing";   fail=1; }
    [ -d "${IMGBUILD_ROOT}/packaging" ]|| { imgbuild::log_error "packaging/ missing"; fail=1; }

    [ -f "${IMGBUILD_ROOT}/overlay/etc/ados/ap-fallback/hostapd.conf.template" ] \
        || { imgbuild::log_error "overlay AP-fallback hostapd.conf.template missing"; fail=1; }
    [ -f "${IMGBUILD_ROOT}/overlay/etc/ados/ap-fallback/dnsmasq.conf" ] \
        || { imgbuild::log_error "overlay AP-fallback dnsmasq.conf missing"; fail=1; }

    # Per-board recipe smoke-check: every boards/<slug>/ must have recipe.sh + board.yaml.
    for d in "${IMGBUILD_ROOT}/boards"/*/; do
        [ -d "${d}" ] || continue
        local slug; slug="$(basename "${d}")"
        [ -f "${d}/recipe.sh" ] || { imgbuild::log_error "boards/${slug}/recipe.sh missing"; fail=1; }
        [ -f "${d}/board.yaml" ] || { imgbuild::log_error "boards/${slug}/board.yaml missing"; fail=1; }

        # Validate board.yaml is YAML-parseable if python3 is on PATH.
        if command -v python3 >/dev/null 2>&1 && [ -f "${d}/board.yaml" ]; then
            python3 -c "import sys, yaml; yaml.safe_load(open(sys.argv[1]))" "${d}/board.yaml" \
                2>/dev/null || { imgbuild::log_error "boards/${slug}/board.yaml is not valid YAML"; fail=1; }
        fi
    done

    if [ "${fail}" -eq 0 ]; then
        imgbuild::log_ok "imagebuilder/ scaffolding looks healthy."
        return 0
    else
        imgbuild::log_error "imagebuilder/ scaffolding has issues — fix above before building."
        return 1
    fi
}

# ---------------------------------------------------------------------
# Recipe driver — sources a board's recipe.sh and runs the hooks.
# ---------------------------------------------------------------------

imgbuild::run_recipe() {
    local slug="$1"
    local board_dir="${IMGBUILD_ROOT}/boards/${slug}"
    local recipe="${board_dir}/recipe.sh"

    if [ ! -f "${recipe}" ]; then
        imgbuild::log_error "no recipe.sh at ${recipe}"
        return 1
    fi

    # Per-build scratch space + output dir.
    BOARD_SLUG="${slug}"
    BOARD_DIR="${board_dir}"
    SDK_DIR="$(mktemp -d -t "ados-imgbuild-${slug}-XXXXXX")"
    OUTPUT_DIR="${IMGBUILD_OUTPUT}/${slug}"
    mkdir -p "${OUTPUT_DIR}"
    export BOARD_SLUG BOARD_DIR SDK_DIR OUTPUT_DIR

    # Cleanup hook — keep SDK_DIR on failure for debugging unless
    # IMGBUILD_KEEP_SDK is unset and the run succeeded.
    trap 'rc=$?; if [ "${rc}" -ne 0 ]; then imgbuild::log_warn "leaving SDK at ${SDK_DIR} for debug"; else [ -z "${IMGBUILD_KEEP_SDK:-}" ] && rm -rf "${SDK_DIR}"; fi; exit "${rc}"' EXIT

    imgbuild::log_step "Building ${slug} (version ${IMGBUILD_VERSION})…"

    # shellcheck source=/dev/null
    . "${recipe}"

    # Optional: declare board-defined VERSION override.
    : "${VERSION:=${IMGBUILD_VERSION}}"
    export VERSION

    imgbuild::log_step "[1/8] sdk_clone"     ; recipe::sdk_clone
    imgbuild::log_step "[2/8] sdk_configure" ; recipe::sdk_configure

    if declare -F recipe::build_drivers >/dev/null; then
        imgbuild::log_step "[3/8] build_drivers (optional)"
        recipe::build_drivers
    else
        imgbuild::log_info "[3/8] build_drivers — recipe declined the hook"
    fi

    imgbuild::log_step "[4/8] sdk_build"     ; recipe::sdk_build

    if declare -F recipe::pre_overlay >/dev/null; then
        imgbuild::log_step "[5/8] pre_overlay"
        recipe::pre_overlay
    fi

    if [ -z "${ROOTFS_DIR:-}" ]; then
        imgbuild::log_error "recipe did not set ROOTFS_DIR — overlay step has nowhere to land"
        return 1
    fi
    imgbuild::log_step "[6/8] overlay_into ${ROOTFS_DIR}"
    imgbuild::overlay_into "${ROOTFS_DIR}"

    if declare -F recipe::post_overlay >/dev/null; then
        imgbuild::log_step "[7/8] post_overlay"
        recipe::post_overlay
    fi

    imgbuild::log_step "[8/8] stage_image"   ; recipe::stage_image

    # Sign + emit canonical sidecar files.
    imgbuild::publish_artifacts "${slug}"

    imgbuild::log_ok "Built ${slug} successfully — artifacts in ${OUTPUT_DIR}"
}

# ---------------------------------------------------------------------
# Universal overlay rsync — same on every board.
# ---------------------------------------------------------------------

imgbuild::overlay_into() {
    local target="$1"
    [ -d "${target}" ] || { imgbuild::log_error "overlay target dir does not exist: ${target}"; return 1; }

    if ! command -v rsync >/dev/null 2>&1; then
        imgbuild::log_error "rsync not on PATH — install it before running this orchestrator"
        return 1
    fi

    rsync -a --info=NAME "${IMGBUILD_ROOT}/overlay/" "${target}/"
    imgbuild::log_ok "overlay rsynced into ${target}"
}

# ---------------------------------------------------------------------
# Agent binary fetcher — pulls + verifies signed tarball from the
# rolling lite-agent-main GitHub Release.
#
# Args:
#   $1 — target triple (e.g. armv7-unknown-linux-musleabihf)
#   $2 — destination path for the binary
# ---------------------------------------------------------------------

imgbuild::download_agent_binary() {
    local triple="$1"
    local dest="$2"
    local tmp; tmp="$(mktemp -d)"
    trap 'rm -rf "${tmp}"' RETURN

    if ! command -v gh >/dev/null 2>&1; then
        imgbuild::log_error "gh CLI not on PATH — install it before running this orchestrator"
        return 1
    fi

    imgbuild::log_step "Downloading lite-agent binary for ${triple}…"
    (
        cd "${tmp}"
        gh release download "${IMGBUILD_AGENT_RELEASE_TAG}" \
            --repo "${IMGBUILD_AGENT_REPO}" \
            --pattern "*${triple}*.tar.gz" \
            --pattern "*${triple}*.tar.gz.minisig" \
            --pattern "SHA256SUMS"
    )

    local tarball; tarball=$(ls "${tmp}"/*"${triple}"*.tar.gz | head -n1)
    if [ -z "${tarball}" ] || [ ! -f "${tarball}" ]; then
        imgbuild::log_error "no agent tarball matched triple=${triple} in release ${IMGBUILD_AGENT_RELEASE_TAG}"
        return 1
    fi

    # Verify minisig if available.
    local sig="${tarball}.minisig"
    if [ -f "${sig}" ]; then
        local pubkey
        pubkey=$(grep -oE 'RW[A-Za-z0-9+/=]+' "${REPO_ROOT}/scripts/install-lite.sh" | head -n1)
        if [ -n "${pubkey}" ] && command -v minisign >/dev/null 2>&1; then
            minisign -V -P "${pubkey}" -m "${tarball}" || {
                imgbuild::log_error "minisign verify FAILED for ${tarball}"
                return 1
            }
            imgbuild::log_ok "minisig verified"
        else
            imgbuild::log_warn "skipping minisign verify (minisign or pubkey not available)"
        fi
    else
        imgbuild::log_warn "no .minisig sidecar found — accepting tarball unverified"
    fi

    # Extract and place at dest.
    mkdir -p "$(dirname "${dest}")"
    tar -xzOf "${tarball}" '*/ados-agent-lite' > "${dest}" 2>/dev/null || \
        tar -xzOf "${tarball}" 'ados-agent-lite' > "${dest}"
    chmod 0755 "${dest}"
    imgbuild::log_ok "installed agent binary at ${dest}"
}

# ---------------------------------------------------------------------
# Sign + sha256 + write SHA256SUMS sidecar — runs after stage_image.
# ---------------------------------------------------------------------

imgbuild::publish_artifacts() {
    local slug="$1"
    local artifact; artifact=$(ls "${OUTPUT_DIR}"/ados-"${slug}"-*.img.gz 2>/dev/null | head -n1)

    if [ -z "${artifact}" ] || [ ! -f "${artifact}" ]; then
        imgbuild::log_error "stage_image did not produce ${OUTPUT_DIR}/ados-${slug}-*.img.gz"
        return 1
    fi

    # Always emit sha256.
    if [ ! -f "${artifact}.sha256" ]; then
        ( cd "${OUTPUT_DIR}" && sha256sum "$(basename "${artifact}")" > "$(basename "${artifact}").sha256" )
    fi

    # Sign if we have the secret in env (CI path).
    if [ -n "${LITE_AGENT_MINISIGN_KEY:-}" ] && command -v minisign >/dev/null 2>&1; then
        local key_file; key_file="$(mktemp)"
        printf '%s' "${LITE_AGENT_MINISIGN_KEY}" > "${key_file}"
        echo "${LITE_AGENT_MINISIGN_PASSWORD:-}" \
            | minisign -S -s "${key_file}" -m "${artifact}"
        shred -u "${key_file}" 2>/dev/null || rm -f "${key_file}"
        imgbuild::log_ok "signed ${artifact}"
    else
        imgbuild::log_warn "LITE_AGENT_MINISIGN_KEY not set — image will ship unsigned"
    fi

    # SHA256SUMS sidecar covering all artifacts in the OUTPUT_DIR.
    ( cd "${OUTPUT_DIR}" && sha256sum ./*.img.gz ./*.minisig 2>/dev/null > SHA256SUMS || true )

    imgbuild::log_info "Artifacts:"
    ls -la "${OUTPUT_DIR}/" >&2
}

# ---------------------------------------------------------------------
# Cross-build helpers used by per-board recipes.
# ---------------------------------------------------------------------

# Cross-build an out-of-tree kernel module against a specific kernel
# tree + cross-toolchain. Used by recipe::build_drivers().
#
# Args:
#   $1 — module source dir (must contain a Makefile compatible with kbuild)
#   $2 — kernel object dir (e.g. ${SDK_DIR}/sysdrv/source/objs_kernel)
#   $3 — toolchain prefix (full path to the .../bin/<triple>- prefix)
imgbuild::cross_build_module() {
    local src="$1" kdir="$2" cross="$3"
    [ -d "${src}" ] || { imgbuild::log_error "module src missing: ${src}"; return 1; }
    [ -d "${kdir}" ] || { imgbuild::log_error "kernel dir missing: ${kdir}"; return 1; }
    imgbuild::log_step "Cross-building $(basename "${src}") against ${kdir}"
    make -C "${src}" \
        ARCH=arm \
        CROSS_COMPILE="${cross}" \
        KSRC="${kdir}" \
        KDIR="${kdir}" \
        all
}

# Cross-build a plain C binary against a vendor toolchain.
#
# Args:
#   $1 — source dir
#   $2 — toolchain root (path containing bin/<triple>-gcc)
#   $3 — pass-through args appended to make (e.g. "SDK_ROOT=...")
imgbuild::cross_build_c() {
    local src="$1" toolchain="$2"
    shift 2
    [ -d "${src}" ] || { imgbuild::log_error "src missing: ${src}"; return 1; }
    imgbuild::log_step "Cross-building C source at ${src}"
    PATH="${toolchain}/bin:${PATH}" make -C "${src}" "$@"
}
