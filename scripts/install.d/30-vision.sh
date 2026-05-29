# shellcheck shell=bash
# =============================================================================
# 30-vision.sh — install the prebuilt ados-vision binary.
#
# CI builds the vision engine as a static arm64 binary and publishes it to the
# rolling 'prebuilt-vision' prerelease; here we fetch it and verify it through
# the shared artifact verifier (SHA256 always; an Ed25519/minisign signature is
# enforced automatically once a key + .minisig are published).
#
# Best-effort by design: if the fetch or verification fails (offline, no asset
# yet, non-arm64 host), the install continues. The ados-vision unit execs the
# binary when present and fails loudly otherwise, so a missing binary surfaces
# as a failed ados-vision unit in journald. Idempotent.
# =============================================================================

ADOS_VISION_RELEASE_BASE="https://github.com/altnautica/ADOSDroneAgent/releases/download/prebuilt-vision"
ADOS_VISION_ASSET="ados-vision-aarch64"
ADOS_VISION_PUBKEY="${ADOS_VISION_PUBKEY:-}"

if ! command -v ados_verify_artifact >/dev/null 2>&1; then
    _ados_vision_lib="$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" 2>/dev/null && pwd || true)"
    if [ -n "${_ados_vision_lib}" ] && [ -f "${_ados_vision_lib}/verify.sh" ]; then
        # shellcheck source=/dev/null
        . "${_ados_vision_lib}/verify.sh"
    fi
fi

install_vision_binary() {
    local arch
    arch="$(uname -m)"
    if [ "${arch}" != "aarch64" ] && [ "${arch}" != "arm64" ]; then
        warn "ados-vision prebuilt is arm64 only; skipping on ${arch}."
        return 0
    fi

    local bin_dir="${INSTALL_DIR}/bin"
    local dest="${bin_dir}/ados-vision"
    local tmp
    tmp="$(mktemp -d)"
    install -d -m 0755 "${bin_dir}"

    if ! curl -fsSL "${ADOS_VISION_RELEASE_BASE}/${ADOS_VISION_ASSET}" -o "${tmp}/${ADOS_VISION_ASSET}" \
        || ! curl -fsSL "${ADOS_VISION_RELEASE_BASE}/${ADOS_VISION_ASSET}.sha256" -o "${tmp}/${ADOS_VISION_ASSET}.sha256"; then
        warn "Could not fetch the ados-vision prebuilt; skipping."
        rm -rf "${tmp}"
        return 0
    fi
    curl -fsSL "${ADOS_VISION_RELEASE_BASE}/${ADOS_VISION_ASSET}.minisig" -o "${tmp}/${ADOS_VISION_ASSET}.minisig" 2>/dev/null || true

    if command -v ados_verify_artifact >/dev/null 2>&1; then
        if ! ados_verify_artifact "${tmp}/${ADOS_VISION_ASSET}" "${ADOS_VISION_PUBKEY}" "edge" 0; then
            warn "ados-vision failed verification; not installing the binary."
            rm -rf "${tmp}"
            return 0
        fi
    elif ! ( cd "${tmp}" && sha256sum -c "${ADOS_VISION_ASSET}.sha256" >/dev/null 2>&1 ); then
        warn "ados-vision checksum mismatch; not installing the binary."
        rm -rf "${tmp}"
        return 0
    fi

    install -m 0755 "${tmp}/${ADOS_VISION_ASSET}" "${dest}"
    rm -rf "${tmp}"
    info "Vision engine binary installed: ${dest}"

    # The unit may already be running from a prior install. On an upgrade that
    # refreshes the binary, restart a running ados-vision so it picks up the new
    # build. On a fresh install the unit is not up yet (the supervisor starts it
    # later), so this is a no-op there.
    if systemctl is-active --quiet ados-vision 2>/dev/null; then
        systemctl restart ados-vision 2>/dev/null || true
        info "Vision service restarted onto the installed native binary."
    fi
}
export -f install_vision_binary
