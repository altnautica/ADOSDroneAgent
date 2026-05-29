# shellcheck shell=bash
# =============================================================================
# 22-cloud.sh — install the prebuilt ados-cloud relay binary (+ the ados-ota
# oneshot poller).
#
# CI builds the cloud relay as a static arm64 binary and publishes it to the
# rolling 'prebuilt-cloud' prerelease; here we fetch it and verify it through
# the shared artifact verifier (SHA256 always; an Ed25519/minisign signature is
# enforced automatically once a key + .minisig are published).
#
# Best-effort by design: if the fetch or verification fails (offline, no asset
# yet, non-arm64 host), the install continues. Placing the binary on disk is
# inert: the ados-cloud unit's ExecStart shim runs the native binary only when
# the operator both enabled the flag file (/etc/ados/cloud-rust-enabled) and the
# binary is present; otherwise the packaged Python cloud service is the default.
# Idempotent: re-running re-fetches and overwrites.
# =============================================================================

ADOS_CLOUD_RELEASE_BASE="https://github.com/altnautica/ADOSDroneAgent/releases/download/prebuilt-cloud"
ADOS_CLOUD_ASSET="ados-cloud-aarch64"
ADOS_OTA_ASSET="ados-ota-aarch64"
# Public key for signature verification. Empty until a signing key is
# provisioned; with it empty the edge channel is SHA256-checked only.
ADOS_CLOUD_PUBKEY="${ADOS_CLOUD_PUBKEY:-}"

# Bring in the shared verifier (resolve relative to this module's location).
if ! command -v ados_verify_artifact >/dev/null 2>&1; then
    _ados_cloud_lib="$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" 2>/dev/null && pwd || true)"
    if [ -n "${_ados_cloud_lib}" ] && [ -f "${_ados_cloud_lib}/verify.sh" ]; then
        # shellcheck source=/dev/null
        . "${_ados_cloud_lib}/verify.sh"
    fi
fi

# Fetch + verify + install one prebuilt asset into INSTALL_DIR/bin. Returns 0
# even on failure (best-effort), printing a warning. Echoes nothing.
_install_cloud_asset() {
    local asset="$1"
    local bin_dir="${INSTALL_DIR}/bin"
    local dest="${bin_dir}/${asset%-aarch64}"
    local tmp
    tmp="$(mktemp -d)"
    install -d -m 0755 "${bin_dir}"

    if ! curl -fsSL "${ADOS_CLOUD_RELEASE_BASE}/${asset}" -o "${tmp}/${asset}" \
        || ! curl -fsSL "${ADOS_CLOUD_RELEASE_BASE}/${asset}.sha256" -o "${tmp}/${asset}.sha256"; then
        warn "Could not fetch the ${asset} prebuilt; skipping."
        rm -rf "${tmp}"
        return 0
    fi
    curl -fsSL "${ADOS_CLOUD_RELEASE_BASE}/${asset}.minisig" -o "${tmp}/${asset}.minisig" 2>/dev/null || true

    if command -v ados_verify_artifact >/dev/null 2>&1; then
        if ! ados_verify_artifact "${tmp}/${asset}" "${ADOS_CLOUD_PUBKEY}" "edge" 0; then
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
    info "Cloud binary installed: ${dest}"
}

install_cloud_binary() {
    local arch
    arch="$(uname -m)"
    if [ "${arch}" != "aarch64" ] && [ "${arch}" != "arm64" ]; then
        warn "ados-cloud prebuilt is arm64 only; skipping on ${arch}."
        return 0
    fi

    _install_cloud_asset "${ADOS_CLOUD_ASSET}"
    _install_cloud_asset "${ADOS_OTA_ASSET}"

    # The unit ships with the Python service as the default; the native relay is
    # opt-in. Only re-assert a restart when the operator has already enabled the
    # flag AND the service is running, so a routine upgrade that refreshes the
    # binary never flips a Python-default deployment onto the native binary.
    if [ -e /etc/ados/cloud-rust-enabled ] && systemctl is-active --quiet ados-cloud 2>/dev/null; then
        systemctl restart ados-cloud 2>/dev/null || true
        info "Cloud relay restarted onto the installed binary."
    fi
}
export -f install_cloud_binary
