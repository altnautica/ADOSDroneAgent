# shellcheck shell=bash
# =============================================================================
# 26-display.sh — install the prebuilt display daemons (ados-display +
# ados-display-probe).
#
# CI builds both binaries from the ados-display crate as static arm64 binaries
# and publishes them to the rolling 'prebuilt-display' prerelease; here we fetch
# each and verify it through the shared artifact verifier (SHA256 always; an
# Ed25519/minisign signature is enforced automatically once a key + .minisig are
# published).
#
# Best-effort by design: if a fetch or verification fails (offline, no asset yet,
# non-arm64 host), the install continues and the ados-oled / ados-display-probe
# units' ExecStart shims run the packaged Python services. Placing the binaries
# on disk is inert: the shims run them only when the operator both enabled the
# flag file (/etc/ados/display-rust-enabled) and the binary is present; otherwise
# the packaged Python services are the default. Idempotent: re-running re-fetches
# and overwrites.
# =============================================================================

ADOS_DISPLAY_RELEASE_BASE="https://github.com/altnautica/ADOSDroneAgent/releases/download/prebuilt-display"
ADOS_DISPLAY_ASSET="ados-display-aarch64"
ADOS_DISPLAY_PROBE_ASSET="ados-display-probe-aarch64"
# Public key for signature verification. Empty until a signing key is
# provisioned; with it empty the edge channel is SHA256-checked only.
ADOS_DISPLAY_PUBKEY="${ADOS_DISPLAY_PUBKEY:-}"

# Bring in the shared verifier (resolve relative to this module's location).
if ! command -v ados_verify_artifact >/dev/null 2>&1; then
    _ados_disp_lib="$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" 2>/dev/null && pwd || true)"
    if [ -n "${_ados_disp_lib}" ] && [ -f "${_ados_disp_lib}/verify.sh" ]; then
        # shellcheck source=/dev/null
        . "${_ados_disp_lib}/verify.sh"
    fi
fi

# Fetch + verify + install one prebuilt asset into INSTALL_DIR/bin. Returns 0
# even on failure (best-effort), printing a warning. Echoes nothing.
_install_display_asset() {
    local asset="$1"
    local bin_dir="${INSTALL_DIR}/bin"
    local dest="${bin_dir}/${asset%-aarch64}"
    local tmp
    tmp="$(mktemp -d)"
    install -d -m 0755 "${bin_dir}"

    if ! curl -fsSL "${ADOS_DISPLAY_RELEASE_BASE}/${asset}" -o "${tmp}/${asset}" \
        || ! curl -fsSL "${ADOS_DISPLAY_RELEASE_BASE}/${asset}.sha256" -o "${tmp}/${asset}.sha256"; then
        warn "Could not fetch the ${asset} prebuilt; skipping."
        rm -rf "${tmp}"
        return 0
    fi
    curl -fsSL "${ADOS_DISPLAY_RELEASE_BASE}/${asset}.minisig" -o "${tmp}/${asset}.minisig" 2>/dev/null || true

    if command -v ados_verify_artifact >/dev/null 2>&1; then
        if ! ados_verify_artifact "${tmp}/${asset}" "${ADOS_DISPLAY_PUBKEY}" "edge" 0; then
            warn "${asset} failed verification; not installing the binary."
            rm -rf "${tmp}"
            return 0
        fi
    elif ! ( cd "${tmp}" && sha256sum -c "${asset}.sha256" >/dev/null 2>&1 ); then
        warn "${asset} checksum mismatch; not installing the binary."
        rm -rf "${tmp}"
        return 0
    fi

    install -m 0755 "${tmp}/${asset}" "${dest}"
    rm -rf "${tmp}"
    info "Display binary installed: ${dest}"
}

install_display_binary() {
    local arch
    arch="$(uname -m)"
    if [ "${arch}" != "aarch64" ] && [ "${arch}" != "arm64" ]; then
        warn "ados-display prebuilts are arm64 only; skipping on ${arch}."
        return 0
    fi

    _install_display_asset "${ADOS_DISPLAY_ASSET}"
    _install_display_asset "${ADOS_DISPLAY_PROBE_ASSET}"

    # The native binaries are OPT-IN: re-assert the running display writer onto
    # it ONLY when the operator has enabled the flag. Without the flag the unit
    # keeps running the Python service, so a routine upgrade never bounces the
    # display for a binary it would not select anyway. The probe is a boot-time
    # oneshot, so it is not restarted here.
    if [ -e "${CONFIG_DIR}/display-rust-enabled" ] && systemctl is-active --quiet ados-oled 2>/dev/null; then
        systemctl restart ados-oled 2>/dev/null || true
        info "Display writer restarted onto the installed binary (flag enabled)."
    fi
}
export -f install_display_binary
