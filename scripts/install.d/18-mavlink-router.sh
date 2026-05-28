# shellcheck shell=bash
# =============================================================================
# 18-mavlink-router.sh — install the prebuilt ados-mavlink-router binary.
#
# CI builds the MAVLink router as a static arm64 binary and publishes it to the
# rolling 'prebuilt-mavlink-router' prerelease; here we fetch it and verify it
# through the shared artifact verifier (SHA256 always; an Ed25519/minisign
# signature is enforced automatically once a key + .minisig are published).
#
# Best-effort by design: if the fetch or verification fails (offline, no asset
# yet, non-arm64 host), the install continues and the ados-mavlink unit's
# ExecStart shim runs the packaged Python service. Idempotent.
# =============================================================================

ADOS_MAVROUTER_RELEASE_BASE="https://github.com/altnautica/ADOSDroneAgent/releases/download/prebuilt-mavlink-router"
ADOS_MAVROUTER_ASSET="ados-mavlink-router-aarch64"
ADOS_MAVROUTER_PUBKEY="${ADOS_MAVROUTER_PUBKEY:-}"

if ! command -v ados_verify_artifact >/dev/null 2>&1; then
    _ados_mavr_lib="$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" 2>/dev/null && pwd || true)"
    if [ -n "${_ados_mavr_lib}" ] && [ -f "${_ados_mavr_lib}/verify.sh" ]; then
        # shellcheck source=/dev/null
        . "${_ados_mavr_lib}/verify.sh"
    fi
fi

install_mavlink_router_binary() {
    local arch
    arch="$(uname -m)"
    if [ "${arch}" != "aarch64" ] && [ "${arch}" != "arm64" ]; then
        warn "ados-mavlink-router prebuilt is arm64 only; skipping on ${arch}."
        return 0
    fi

    local bin_dir="${INSTALL_DIR}/bin"
    local dest="${bin_dir}/ados-mavlink-router"
    local tmp
    tmp="$(mktemp -d)"
    install -d -m 0755 "${bin_dir}"

    if ! curl -fsSL "${ADOS_MAVROUTER_RELEASE_BASE}/${ADOS_MAVROUTER_ASSET}" -o "${tmp}/${ADOS_MAVROUTER_ASSET}" \
        || ! curl -fsSL "${ADOS_MAVROUTER_RELEASE_BASE}/${ADOS_MAVROUTER_ASSET}.sha256" -o "${tmp}/${ADOS_MAVROUTER_ASSET}.sha256"; then
        warn "Could not fetch the ados-mavlink-router prebuilt; skipping."
        rm -rf "${tmp}"
        return 0
    fi
    curl -fsSL "${ADOS_MAVROUTER_RELEASE_BASE}/${ADOS_MAVROUTER_ASSET}.minisig" -o "${tmp}/${ADOS_MAVROUTER_ASSET}.minisig" 2>/dev/null || true

    if command -v ados_verify_artifact >/dev/null 2>&1; then
        if ! ados_verify_artifact "${tmp}/${ADOS_MAVROUTER_ASSET}" "${ADOS_MAVROUTER_PUBKEY}" "edge" 0; then
            warn "ados-mavlink-router failed verification; not installing the binary."
            rm -rf "${tmp}"
            return 0
        fi
    elif ! ( cd "${tmp}" && sha256sum -c "${ADOS_MAVROUTER_ASSET}.sha256" >/dev/null 2>&1 ); then
        warn "ados-mavlink-router checksum mismatch; not installing the binary."
        rm -rf "${tmp}"
        return 0
    fi

    install -m 0755 "${tmp}/${ADOS_MAVROUTER_ASSET}" "${dest}"
    rm -rf "${tmp}"
    info "MAVLink router binary installed: ${dest}"

    # The unit may have restarted earlier in the install before this binary was
    # on disk, so its ExecStart shim would have selected the Python fallback.
    # Re-assert the running service now that the binary is present so the install
    # lands on the native router with no manual restart. Only when already active
    # (a fresh install starts it later, with the binary present).
    if systemctl is-active --quiet ados-mavlink 2>/dev/null; then
        systemctl restart ados-mavlink 2>/dev/null || true
        info "MAVLink service restarted onto the installed binary."
    fi
}
export -f install_mavlink_router_binary
