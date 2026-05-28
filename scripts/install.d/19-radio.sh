# shellcheck shell=bash
# =============================================================================
# 19-radio.sh — install the prebuilt ados-radio WFB TX binary.
#
# CI builds the WFB radio manager as a static arm64 binary and publishes it to
# the rolling 'prebuilt-radio' prerelease; here we fetch it and verify it
# through the shared artifact verifier (SHA256 always; an Ed25519/minisign
# signature is enforced automatically once a key + .minisig are published).
#
# Best-effort by design: if the fetch or verification fails (offline, no asset
# yet, non-arm64 host), the install continues and the ados-wfb unit's ExecStart
# shim runs the packaged Python service. Idempotent.
# =============================================================================

ADOS_RADIO_RELEASE_BASE="https://github.com/altnautica/ADOSDroneAgent/releases/download/prebuilt-radio"
ADOS_RADIO_ASSET="ados-radio-aarch64"
ADOS_RADIO_PUBKEY="${ADOS_RADIO_PUBKEY:-}"

if ! command -v ados_verify_artifact >/dev/null 2>&1; then
    _ados_radio_lib="$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" 2>/dev/null && pwd || true)"
    if [ -n "${_ados_radio_lib}" ] && [ -f "${_ados_radio_lib}/verify.sh" ]; then
        # shellcheck source=/dev/null
        . "${_ados_radio_lib}/verify.sh"
    fi
fi

install_radio_binary() {
    local arch
    arch="$(uname -m)"
    if [ "${arch}" != "aarch64" ] && [ "${arch}" != "arm64" ]; then
        warn "ados-radio prebuilt is arm64 only; skipping on ${arch}."
        return 0
    fi

    local bin_dir="${INSTALL_DIR}/bin"
    local dest="${bin_dir}/ados-radio"
    local tmp
    tmp="$(mktemp -d)"
    install -d -m 0755 "${bin_dir}"

    if ! curl -fsSL "${ADOS_RADIO_RELEASE_BASE}/${ADOS_RADIO_ASSET}" -o "${tmp}/${ADOS_RADIO_ASSET}" \
        || ! curl -fsSL "${ADOS_RADIO_RELEASE_BASE}/${ADOS_RADIO_ASSET}.sha256" -o "${tmp}/${ADOS_RADIO_ASSET}.sha256"; then
        warn "Could not fetch the ados-radio prebuilt; skipping."
        rm -rf "${tmp}"
        return 0
    fi
    curl -fsSL "${ADOS_RADIO_RELEASE_BASE}/${ADOS_RADIO_ASSET}.minisig" -o "${tmp}/${ADOS_RADIO_ASSET}.minisig" 2>/dev/null || true

    if command -v ados_verify_artifact >/dev/null 2>&1; then
        if ! ados_verify_artifact "${tmp}/${ADOS_RADIO_ASSET}" "${ADOS_RADIO_PUBKEY}" "edge" 0; then
            warn "ados-radio failed verification; not installing the binary."
            rm -rf "${tmp}"
            return 0
        fi
    elif ! ( cd "${tmp}" && sha256sum -c "${ADOS_RADIO_ASSET}.sha256" >/dev/null 2>&1 ); then
        warn "ados-radio checksum mismatch; not installing the binary."
        rm -rf "${tmp}"
        return 0
    fi

    install -m 0755 "${tmp}/${ADOS_RADIO_ASSET}" "${dest}"
    rm -rf "${tmp}"
    info "WFB radio binary installed: ${dest}"

    # The unit may have restarted earlier in the install before this binary was
    # on disk, so its ExecStart shim would have selected the Python fallback.
    # Re-assert the running service (drone profile only) now that the binary is
    # present so the install lands on the native binary with no manual restart.
    if systemctl is-active --quiet ados-wfb 2>/dev/null; then
        systemctl restart ados-wfb 2>/dev/null || true
        info "WFB service restarted onto the installed binary."
    fi
}
export -f install_radio_binary
