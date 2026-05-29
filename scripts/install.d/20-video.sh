# shellcheck shell=bash
# =============================================================================
# 20-video.sh — install the prebuilt ados-video orchestrator binary.
#
# CI builds the video pipeline orchestrator as a static arm64 binary and
# publishes it to the rolling 'prebuilt-video' prerelease; here we fetch it and
# verify it through the shared artifact verifier (SHA256 always; an
# Ed25519/minisign signature is enforced automatically once a key + .minisig
# are published).
#
# The native binary is the ONLY video orchestrator now (the standalone Python
# service was removed once it was bench-validated), so the ados-video unit execs
# it unconditionally and fails loudly if it is absent. The fetch is still
# best-effort here (a transient offline/no-asset case must not abort the whole
# install); a missing binary surfaces as a failed ados-video unit in journald.
# Idempotent.
# =============================================================================

ADOS_VIDEO_RELEASE_BASE="https://github.com/altnautica/ADOSDroneAgent/releases/download/prebuilt-video"
ADOS_VIDEO_ASSET="ados-video-aarch64"
ADOS_VIDEO_PUBKEY="${ADOS_VIDEO_PUBKEY:-}"

if ! command -v ados_verify_artifact >/dev/null 2>&1; then
    _ados_video_lib="$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" 2>/dev/null && pwd || true)"
    if [ -n "${_ados_video_lib}" ] && [ -f "${_ados_video_lib}/verify.sh" ]; then
        # shellcheck source=/dev/null
        . "${_ados_video_lib}/verify.sh"
    fi
fi

install_video_binary() {
    local arch
    arch="$(uname -m)"
    if [ "${arch}" != "aarch64" ] && [ "${arch}" != "arm64" ]; then
        warn "ados-video prebuilt is arm64 only; skipping on ${arch}."
        return 0
    fi

    local bin_dir="${INSTALL_DIR}/bin"
    local dest="${bin_dir}/ados-video"
    local tmp
    tmp="$(mktemp -d)"
    install -d -m 0755 "${bin_dir}"

    if ! curl -fsSL "${ADOS_VIDEO_RELEASE_BASE}/${ADOS_VIDEO_ASSET}" -o "${tmp}/${ADOS_VIDEO_ASSET}" \
        || ! curl -fsSL "${ADOS_VIDEO_RELEASE_BASE}/${ADOS_VIDEO_ASSET}.sha256" -o "${tmp}/${ADOS_VIDEO_ASSET}.sha256"; then
        warn "Could not fetch the ados-video prebuilt; skipping."
        rm -rf "${tmp}"
        return 0
    fi
    curl -fsSL "${ADOS_VIDEO_RELEASE_BASE}/${ADOS_VIDEO_ASSET}.minisig" -o "${tmp}/${ADOS_VIDEO_ASSET}.minisig" 2>/dev/null || true

    if command -v ados_verify_artifact >/dev/null 2>&1; then
        if ! ados_verify_artifact "${tmp}/${ADOS_VIDEO_ASSET}" "${ADOS_VIDEO_PUBKEY}" "edge" 0; then
            warn "ados-video failed verification; not installing the binary."
            rm -rf "${tmp}"
            return 0
        fi
    elif ! ( cd "${tmp}" && sha256sum -c "${ADOS_VIDEO_ASSET}.sha256" >/dev/null 2>&1 ); then
        warn "ados-video checksum mismatch; not installing the binary."
        rm -rf "${tmp}"
        return 0
    fi

    install -m 0755 "${tmp}/${ADOS_VIDEO_ASSET}" "${dest}"
    rm -rf "${tmp}"
    info "Video orchestrator binary installed: ${dest}"

    # The native binary is the only video orchestrator now. On an upgrade that
    # refreshes the binary, restart a running ados-video so it picks up the new
    # build. On a fresh install the unit is not up yet (the supervisor starts it
    # later), so this is a no-op there.
    if systemctl is-active --quiet ados-video 2>/dev/null; then
        systemctl restart ados-video 2>/dev/null || true
        info "Video service restarted onto the installed native binary."
    fi
}
export -f install_video_binary
