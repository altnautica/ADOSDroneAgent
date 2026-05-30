# shellcheck shell=bash
# =============================================================================
# 17-supervisor.sh — install the prebuilt ados-supervisor orchestrator binary.
#
# CI builds the supervisor as a static arm64 binary and publishes it to the
# rolling 'prebuilt-supervisor' prerelease; here we fetch it and verify it
# through the shared artifact verifier (SHA256 always; an Ed25519/minisign
# signature is enforced automatically once a key + .minisig are published,
# with no change here).
#
# Hard install gate on a supported arch: the native binary is the only
# supervisor. If the fetch or verification fails on aarch64/arm64 the install
# FAILS, because there is no Python fallback and a fresh install must never end
# up with no orchestrator. Non-arm64 dev hosts skip cleanly (no prebuilt is
# published for them). Idempotent: re-running re-fetches and overwrites.
# =============================================================================

ADOS_SUPERVISOR_RELEASE_BASE="https://github.com/altnautica/ADOSDroneAgent/releases/download/prebuilt-supervisor"
ADOS_SUPERVISOR_ASSET="ados-supervisor-aarch64"
# Public key for signature verification. Empty until a signing key is
# provisioned; with it empty the edge channel is SHA256-checked only. Setting
# this (and publishing a .minisig from CI) makes a bad signature fatal.
ADOS_SUPERVISOR_PUBKEY="${ADOS_SUPERVISOR_PUBKEY:-}"

# Bring in the shared verifier (resolve relative to this module's location).
if ! command -v ados_verify_artifact >/dev/null 2>&1; then
    _ados_sup_lib="$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" 2>/dev/null && pwd || true)"
    if [ -n "${_ados_sup_lib}" ] && [ -f "${_ados_sup_lib}/verify.sh" ]; then
        # shellcheck source=/dev/null
        . "${_ados_sup_lib}/verify.sh"
    fi
fi

install_supervisor_binary() {
    local arch
    arch="$(uname -m)"
    if [ "${arch}" != "aarch64" ] && [ "${arch}" != "arm64" ]; then
        warn "ados-supervisor prebuilt is arm64 only; skipping on ${arch}."
        return 0
    fi

    local bin_dir="${INSTALL_DIR}/bin"
    local dest="${bin_dir}/ados-supervisor"
    local tmp
    tmp="$(mktemp -d)"
    # Root-owned, not group/world-writable: the supervisor runs as root, so a
    # writable path here would be a root-escalation.
    install -d -m 0755 "${bin_dir}"

    if ! curl -fsSL "${ADOS_SUPERVISOR_RELEASE_BASE}/${ADOS_SUPERVISOR_ASSET}" -o "${tmp}/${ADOS_SUPERVISOR_ASSET}" \
        || ! curl -fsSL "${ADOS_SUPERVISOR_RELEASE_BASE}/${ADOS_SUPERVISOR_ASSET}.sha256" -o "${tmp}/${ADOS_SUPERVISOR_ASSET}.sha256"; then
        error "Could not fetch the ados-supervisor binary on ${arch}; the install cannot complete without it."
        rm -rf "${tmp}"
        return 1
    fi
    # Signature sidecar is optional today; fetch best-effort so verification
    # upgrades to signature-checked automatically once CI starts signing.
    curl -fsSL "${ADOS_SUPERVISOR_RELEASE_BASE}/${ADOS_SUPERVISOR_ASSET}.minisig" -o "${tmp}/${ADOS_SUPERVISOR_ASSET}.minisig" 2>/dev/null || true

    if command -v ados_verify_artifact >/dev/null 2>&1; then
        if ! ados_verify_artifact "${tmp}/${ADOS_SUPERVISOR_ASSET}" "${ADOS_SUPERVISOR_PUBKEY}" "edge" 0; then
            error "ados-supervisor failed verification on ${arch}; refusing to install an unverified orchestrator."
            rm -rf "${tmp}"
            return 1
        fi
    elif ! ( cd "${tmp}" && sha256sum -c "${ADOS_SUPERVISOR_ASSET}.sha256" >/dev/null 2>&1 ); then
        error "ados-supervisor checksum mismatch on ${arch}; refusing to install a corrupt orchestrator."
        rm -rf "${tmp}"
        return 1
    fi

    install -m 0755 "${tmp}/${ADOS_SUPERVISOR_ASSET}" "${dest}"
    rm -rf "${tmp}"
    info "Supervisor binary installed: ${dest}"

    # The unit restarts earlier in the install, when this binary may not yet be
    # on disk, so its ExecStart shim would have selected the packaged fallback.
    # Re-assert the running supervisor now that the binary is present so the
    # install lands fully working with no manual restart. Only when it is
    # already running (a fresh install starts it later, with the binary present).
    if systemctl is-active --quiet ados-supervisor 2>/dev/null; then
        systemctl restart ados-supervisor 2>/dev/null || true
        info "Supervisor restarted onto the installed binary."
    fi
}
export -f install_supervisor_binary
