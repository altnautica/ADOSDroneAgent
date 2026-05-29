# shellcheck shell=bash
# =============================================================================
# 23-groundlink.sh — install the prebuilt ados-groundlink data-plane binary.
#
# CI builds the ground-station data-plane as a static arm64 binary and publishes
# it to the rolling 'prebuilt-groundlink' prerelease; here we fetch it and verify
# it through the shared artifact verifier (SHA256 always; an Ed25519/minisign
# signature is enforced automatically once a key + .minisig are published).
#
# Best-effort by design: if the fetch or verification fails (offline, no asset
# yet, non-arm64 host), the install continues and the ados-wfb-rx unit's
# ExecStart shim runs the packaged Python service. Placing the binary on disk is
# inert: the shim runs it only when the operator both enabled the flag file
# (/etc/ados/groundlink-rust-enabled) and the binary is present; otherwise the
# packaged Python service is the default. Idempotent: re-running re-fetches and
# overwrites.
# =============================================================================

ADOS_GROUNDLINK_RELEASE_BASE="https://github.com/altnautica/ADOSDroneAgent/releases/download/prebuilt-groundlink"
ADOS_GROUNDLINK_ASSET="ados-groundlink-aarch64"
# Public key for signature verification. Empty until a signing key is
# provisioned; with it empty the edge channel is SHA256-checked only.
ADOS_GROUNDLINK_PUBKEY="${ADOS_GROUNDLINK_PUBKEY:-}"

# Bring in the shared verifier (resolve relative to this module's location).
if ! command -v ados_verify_artifact >/dev/null 2>&1; then
    _ados_gl_lib="$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" 2>/dev/null && pwd || true)"
    if [ -n "${_ados_gl_lib}" ] && [ -f "${_ados_gl_lib}/verify.sh" ]; then
        # shellcheck source=/dev/null
        . "${_ados_gl_lib}/verify.sh"
    fi
fi

install_groundlink_binary() {
    local arch
    arch="$(uname -m)"
    if [ "${arch}" != "aarch64" ] && [ "${arch}" != "arm64" ]; then
        warn "ados-groundlink prebuilt is arm64 only; skipping on ${arch}."
        return 0
    fi

    local bin_dir="${INSTALL_DIR}/bin"
    local dest="${bin_dir}/ados-groundlink"
    local tmp
    tmp="$(mktemp -d)"
    install -d -m 0755 "${bin_dir}"

    if ! curl -fsSL "${ADOS_GROUNDLINK_RELEASE_BASE}/${ADOS_GROUNDLINK_ASSET}" -o "${tmp}/${ADOS_GROUNDLINK_ASSET}" \
        || ! curl -fsSL "${ADOS_GROUNDLINK_RELEASE_BASE}/${ADOS_GROUNDLINK_ASSET}.sha256" -o "${tmp}/${ADOS_GROUNDLINK_ASSET}.sha256"; then
        warn "Could not fetch the ados-groundlink prebuilt; skipping."
        rm -rf "${tmp}"
        return 0
    fi
    curl -fsSL "${ADOS_GROUNDLINK_RELEASE_BASE}/${ADOS_GROUNDLINK_ASSET}.minisig" -o "${tmp}/${ADOS_GROUNDLINK_ASSET}.minisig" 2>/dev/null || true

    if command -v ados_verify_artifact >/dev/null 2>&1; then
        if ! ados_verify_artifact "${tmp}/${ADOS_GROUNDLINK_ASSET}" "${ADOS_GROUNDLINK_PUBKEY}" "edge" 0; then
            warn "ados-groundlink failed verification; not installing the binary."
            rm -rf "${tmp}"
            return 0
        fi
    elif ! ( cd "${tmp}" && sha256sum -c "${ADOS_GROUNDLINK_ASSET}.sha256" >/dev/null 2>&1 ); then
        warn "ados-groundlink checksum mismatch; not installing the binary."
        rm -rf "${tmp}"
        return 0
    fi

    install -m 0755 "${tmp}/${ADOS_GROUNDLINK_ASSET}" "${dest}"
    rm -rf "${tmp}"
    info "Ground-station data-plane binary installed: ${dest}"

    # The native binary is OPT-IN: re-assert the running service onto it ONLY
    # when the operator has enabled the flag. Without the flag the unit keeps
    # running the Python service, so a routine upgrade never bounces the receive
    # plane for a binary it would not select anyway.
    if [ -e "${CONFIG_DIR}/groundlink-rust-enabled" ] && systemctl is-active --quiet ados-wfb-rx 2>/dev/null; then
        systemctl restart ados-wfb-rx 2>/dev/null || true
        info "Ground-station data-plane restarted onto the installed binary (flag enabled)."
    fi
}
export -f install_groundlink_binary
