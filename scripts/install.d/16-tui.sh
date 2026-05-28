# shellcheck shell=bash
# =============================================================================
# 16-tui.sh — install the prebuilt ados-tui terminal dashboard binary.
#
# `ados` with no subcommand hands off to this binary for the live dashboard.
# CI builds it as a static arm64 binary and publishes it to the rolling
# 'prebuilt-tui' prerelease; here we fetch it and verify it through the shared
# artifact verifier (SHA256 always; an Ed25519/minisign signature is enforced
# automatically once a key + .minisig are published, with no change here).
#
# Best-effort by design: if the fetch or verification fails (offline, no asset
# yet, non-arm64 host), the install continues and `ados` falls back to its
# one-shot plain status. Idempotent: re-running re-fetches and overwrites.
# =============================================================================

ADOS_TUI_RELEASE_BASE="https://github.com/altnautica/ADOSDroneAgent/releases/download/prebuilt-tui"
ADOS_TUI_ASSET="ados-tui-aarch64"
# Public key for signature verification. Empty until a signing key is
# provisioned; with it empty the edge channel is SHA256-checked only. Setting
# this (and publishing a .minisig from CI) makes a bad signature fatal.
ADOS_TUI_PUBKEY="${ADOS_TUI_PUBKEY:-}"

# Bring in the shared verifier (resolve relative to this module's location).
if ! command -v ados_verify_artifact >/dev/null 2>&1; then
    _ados_tui_lib="$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" 2>/dev/null && pwd || true)"
    if [ -n "${_ados_tui_lib}" ] && [ -f "${_ados_tui_lib}/verify.sh" ]; then
        # shellcheck source=/dev/null
        . "${_ados_tui_lib}/verify.sh"
    fi
fi

install_tui_binary() {
    local arch
    arch="$(uname -m)"
    if [ "${arch}" != "aarch64" ] && [ "${arch}" != "arm64" ]; then
        warn "ados-tui prebuilt is arm64 only; skipping on ${arch} (ados uses plain status)."
        return 0
    fi

    local bin_dir="${INSTALL_DIR}/bin"
    local dest="${bin_dir}/ados-tui"
    local tmp
    tmp="$(mktemp -d)"
    # Root-owned, not group/world-writable: `ados` execs this binary as root
    # under `sudo ados`, so a writable path here would be a root-escalation.
    install -d -m 0755 "${bin_dir}"

    if ! curl -fsSL "${ADOS_TUI_RELEASE_BASE}/${ADOS_TUI_ASSET}" -o "${tmp}/${ADOS_TUI_ASSET}" \
        || ! curl -fsSL "${ADOS_TUI_RELEASE_BASE}/${ADOS_TUI_ASSET}.sha256" -o "${tmp}/${ADOS_TUI_ASSET}.sha256"; then
        warn "Could not fetch the ados-tui prebuilt; skipping (ados uses plain status)."
        rm -rf "${tmp}"
        return 0
    fi
    # Signature sidecar is optional today; fetch best-effort so verification
    # upgrades to signature-checked automatically once CI starts signing.
    curl -fsSL "${ADOS_TUI_RELEASE_BASE}/${ADOS_TUI_ASSET}.minisig" -o "${tmp}/${ADOS_TUI_ASSET}.minisig" 2>/dev/null || true

    if command -v ados_verify_artifact >/dev/null 2>&1; then
        if ! ados_verify_artifact "${tmp}/${ADOS_TUI_ASSET}" "${ADOS_TUI_PUBKEY}" "edge" 0; then
            warn "ados-tui failed verification; not installing the binary."
            rm -rf "${tmp}"
            return 0
        fi
    elif ! ( cd "${tmp}" && sha256sum -c "${ADOS_TUI_ASSET}.sha256" >/dev/null 2>&1 ); then
        warn "ados-tui checksum mismatch; not installing the binary."
        rm -rf "${tmp}"
        return 0
    fi

    install -m 0755 "${tmp}/${ADOS_TUI_ASSET}" "${dest}"
    rm -rf "${tmp}"
    info "Terminal dashboard installed: ${dest}"
}
export -f install_tui_binary
