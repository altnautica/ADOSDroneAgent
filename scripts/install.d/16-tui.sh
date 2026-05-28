# shellcheck shell=bash
# =============================================================================
# 16-tui.sh — install the prebuilt ados-tui terminal dashboard binary.
#
# `ados` with no subcommand hands off to this binary for the live dashboard.
# CI builds it as a static arm64 binary and publishes it to the rolling
# 'prebuilt-tui' prerelease; here we fetch it and verify its SHA256.
#
# Best-effort by design: if the fetch fails (offline, no asset yet, non-arm64
# host), the install continues and `ados` falls back to its one-shot plain
# status. Idempotent: re-running re-fetches and overwrites in place.
# =============================================================================

ADOS_TUI_RELEASE_BASE="https://github.com/altnautica/ADOSDroneAgent/releases/download/prebuilt-tui"
ADOS_TUI_ASSET="ados-tui-aarch64"

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
    mkdir -p "${bin_dir}"

    if ! curl -fsSL "${ADOS_TUI_RELEASE_BASE}/${ADOS_TUI_ASSET}" -o "${tmp}/${ADOS_TUI_ASSET}" \
        || ! curl -fsSL "${ADOS_TUI_RELEASE_BASE}/${ADOS_TUI_ASSET}.sha256" -o "${tmp}/${ADOS_TUI_ASSET}.sha256"; then
        warn "Could not fetch the ados-tui prebuilt; skipping (ados uses plain status)."
        rm -rf "${tmp}"
        return 0
    fi

    # The published checksum file is 'HASH  ados-tui-aarch64'; verify in place.
    if ! ( cd "${tmp}" && sha256sum -c "${ADOS_TUI_ASSET}.sha256" >/dev/null 2>&1 ); then
        warn "ados-tui checksum mismatch; not installing the binary."
        rm -rf "${tmp}"
        return 0
    fi

    install -m 0755 "${tmp}/${ADOS_TUI_ASSET}" "${dest}"
    rm -rf "${tmp}"
    info "Terminal dashboard installed: ${dest}"
}
export -f install_tui_binary
