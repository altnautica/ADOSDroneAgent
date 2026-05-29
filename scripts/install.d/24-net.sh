# shellcheck shell=bash
# =============================================================================
# 24-net.sh — install the prebuilt ados-net uplink-matrix binary.
#
# CI builds the ground-station uplink matrix as a static arm64 binary and
# publishes it to the rolling 'prebuilt-net' prerelease; here we fetch it and
# verify it through the shared artifact verifier (SHA256 always; an
# Ed25519/minisign signature is enforced automatically once a key + .minisig are
# published).
#
# Best-effort by design: if the fetch or verification fails (offline, no asset
# yet, non-arm64 host), the install continues and the ados-uplink-router unit's
# ExecStart shim runs the packaged Python service. Placing the binary on disk is
# inert: the shim runs it only when the operator both enabled the flag file
# (/etc/ados/net-rust-enabled) and the binary is present; otherwise the packaged
# Python service is the default. The native daemon owns the AP, USB-gadget
# tether, and the ethernet/wifi/modem managers in-process, so enabling its flag
# masks those packaged units (07-systemd.sh). Idempotent: re-running re-fetches
# and overwrites.
# =============================================================================

ADOS_NET_RELEASE_BASE="https://github.com/altnautica/ADOSDroneAgent/releases/download/prebuilt-net"
ADOS_NET_ASSET="ados-net-aarch64"
# Public key for signature verification. Empty until a signing key is
# provisioned; with it empty the edge channel is SHA256-checked only.
ADOS_NET_PUBKEY="${ADOS_NET_PUBKEY:-}"

# Bring in the shared verifier (resolve relative to this module's location).
if ! command -v ados_verify_artifact >/dev/null 2>&1; then
    _ados_net_lib="$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" 2>/dev/null && pwd || true)"
    if [ -n "${_ados_net_lib}" ] && [ -f "${_ados_net_lib}/verify.sh" ]; then
        # shellcheck source=/dev/null
        . "${_ados_net_lib}/verify.sh"
    fi
fi

install_net_binary() {
    local arch
    arch="$(uname -m)"
    if [ "${arch}" != "aarch64" ] && [ "${arch}" != "arm64" ]; then
        warn "ados-net prebuilt is arm64 only; skipping on ${arch}."
        return 0
    fi

    local bin_dir="${INSTALL_DIR}/bin"
    local dest="${bin_dir}/ados-net"
    local tmp
    tmp="$(mktemp -d)"
    install -d -m 0755 "${bin_dir}"

    if ! curl -fsSL "${ADOS_NET_RELEASE_BASE}/${ADOS_NET_ASSET}" -o "${tmp}/${ADOS_NET_ASSET}" \
        || ! curl -fsSL "${ADOS_NET_RELEASE_BASE}/${ADOS_NET_ASSET}.sha256" -o "${tmp}/${ADOS_NET_ASSET}.sha256"; then
        warn "Could not fetch the ados-net prebuilt; skipping."
        rm -rf "${tmp}"
        return 0
    fi
    curl -fsSL "${ADOS_NET_RELEASE_BASE}/${ADOS_NET_ASSET}.minisig" -o "${tmp}/${ADOS_NET_ASSET}.minisig" 2>/dev/null || true

    if command -v ados_verify_artifact >/dev/null 2>&1; then
        if ! ados_verify_artifact "${tmp}/${ADOS_NET_ASSET}" "${ADOS_NET_PUBKEY}" "edge" 0; then
            warn "ados-net failed verification; not installing the binary."
            rm -rf "${tmp}"
            return 0
        fi
    elif ! ( cd "${tmp}" && sha256sum -c "${ADOS_NET_ASSET}.sha256" >/dev/null 2>&1 ); then
        warn "ados-net checksum mismatch; not installing the binary."
        rm -rf "${tmp}"
        return 0
    fi

    install -m 0755 "${tmp}/${ADOS_NET_ASSET}" "${dest}"
    rm -rf "${tmp}"
    info "Uplink matrix binary installed: ${dest}"

    # The native binary is OPT-IN: re-assert the running service onto it ONLY
    # when the operator has enabled the flag. Without the flag the unit keeps
    # running the Python service, so a routine upgrade never bounces the uplink
    # router for a binary it would not select anyway.
    if [ -e "${CONFIG_DIR}/net-rust-enabled" ] && systemctl is-active --quiet ados-uplink-router 2>/dev/null; then
        systemctl restart ados-uplink-router 2>/dev/null || true
        info "Uplink matrix restarted onto the installed binary (flag enabled)."
    fi
}
export -f install_net_binary
