# shellcheck shell=bash
# =============================================================================
# 21-plugin-host.sh — install the prebuilt ados-plugin-host daemon binary.
#
# CI builds the plugin host as a static arm64 binary and publishes it to the
# rolling 'prebuilt-plugin-host' prerelease; here we fetch it and verify it
# through the shared artifact verifier (SHA256 always; an Ed25519/minisign
# signature is enforced automatically once a key + .minisig are published).
#
# Best-effort by design: if the fetch or verification fails (offline, no asset
# yet, non-arm64 host), the install continues. Placing the binary on disk is
# inert: the ados-plugin-host unit ships disabled and its ExecStart shim runs
# the binary only when the operator both enabled the flag file and the binary
# is present. Idempotent: re-running re-fetches and overwrites.
# =============================================================================

ADOS_PLUGIN_HOST_RELEASE_BASE="https://github.com/altnautica/ADOSDroneAgent/releases/download/prebuilt-plugin-host"
ADOS_PLUGIN_HOST_ASSET="ados-plugin-host-aarch64"
# Public key for signature verification. Empty until a signing key is
# provisioned; with it empty the edge channel is SHA256-checked only.
ADOS_PLUGIN_HOST_PUBKEY="${ADOS_PLUGIN_HOST_PUBKEY:-}"

# Bring in the shared verifier (resolve relative to this module's location).
if ! command -v ados_verify_artifact >/dev/null 2>&1; then
    _ados_ph_lib="$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" 2>/dev/null && pwd || true)"
    if [ -n "${_ados_ph_lib}" ] && [ -f "${_ados_ph_lib}/verify.sh" ]; then
        # shellcheck source=/dev/null
        . "${_ados_ph_lib}/verify.sh"
    fi
fi

install_plugin_host_binary() {
    local arch
    arch="$(uname -m)"
    if [ "${arch}" != "aarch64" ] && [ "${arch}" != "arm64" ]; then
        warn "ados-plugin-host prebuilt is arm64 only; skipping on ${arch}."
        return 0
    fi

    local bin_dir="${INSTALL_DIR}/bin"
    local dest="${bin_dir}/ados-plugin-host"
    local tmp
    tmp="$(mktemp -d)"
    install -d -m 0755 "${bin_dir}"

    if ! curl -fsSL "${ADOS_PLUGIN_HOST_RELEASE_BASE}/${ADOS_PLUGIN_HOST_ASSET}" -o "${tmp}/${ADOS_PLUGIN_HOST_ASSET}" \
        || ! curl -fsSL "${ADOS_PLUGIN_HOST_RELEASE_BASE}/${ADOS_PLUGIN_HOST_ASSET}.sha256" -o "${tmp}/${ADOS_PLUGIN_HOST_ASSET}.sha256"; then
        warn "Could not fetch the ados-plugin-host prebuilt; skipping."
        rm -rf "${tmp}"
        return 0
    fi
    curl -fsSL "${ADOS_PLUGIN_HOST_RELEASE_BASE}/${ADOS_PLUGIN_HOST_ASSET}.minisig" -o "${tmp}/${ADOS_PLUGIN_HOST_ASSET}.minisig" 2>/dev/null || true

    if command -v ados_verify_artifact >/dev/null 2>&1; then
        if ! ados_verify_artifact "${tmp}/${ADOS_PLUGIN_HOST_ASSET}" "${ADOS_PLUGIN_HOST_PUBKEY}" "edge" 0; then
            warn "ados-plugin-host failed verification; not installing the binary."
            rm -rf "${tmp}"
            return 0
        fi
    elif ! ( cd "${tmp}" && sha256sum -c "${ADOS_PLUGIN_HOST_ASSET}.sha256" >/dev/null 2>&1 ); then
        warn "ados-plugin-host checksum mismatch; not installing the binary."
        rm -rf "${tmp}"
        return 0
    fi

    install -m 0755 "${tmp}/${ADOS_PLUGIN_HOST_ASSET}" "${dest}"
    rm -rf "${tmp}"
    info "Plugin host binary installed: ${dest}"

    # The unit ships disabled and opt-in. Only re-assert a restart when the
    # operator has already enabled the flag AND the service is running, so a
    # routine upgrade that refreshes the binary never starts a dormant service.
    if [ -e /etc/ados/plugin-host-rust-enabled ] && systemctl is-active --quiet ados-plugin-host 2>/dev/null; then
        systemctl restart ados-plugin-host 2>/dev/null || true
        info "Plugin host restarted onto the installed binary."
    fi
}
export -f install_plugin_host_binary
