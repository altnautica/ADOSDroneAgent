# shellcheck shell=bash
# =============================================================================
# verify.sh — artifact integrity + authenticity for the full agent.
#
# Brings signed-artifact verification to the full agent install (prebuilt
# kernel modules). Sourceable, side-effect-free. SHA256 is always required,
# and so is an Ed25519 (minisign) signature from the release key: the .sha256
# comes from the same host as the artifact, so on its own it proves the
# transfer and not the origin. A tampered, missing or unverifiable signature
# (no .minisig, or minisign absent) is refused on every channel.
#
# Functions (all return 0/non-zero, never exit — callers decide fatality):
#   ados_verify_sha256 ARTIFACT
#       Verify ARTIFACT against ARTIFACT.sha256 (sha256sum -c).
#   ados_verify_minisign ARTIFACT PUBKEY
#       Verify ARTIFACT against ARTIFACT.minisig. Return codes:
#         0 verified | 1 signature INVALID (tamper) | 2 minisign missing |
#         3 signature file missing.
#   ados_verify_artifact ARTIFACT PUBKEY
#       Orchestrate SHA256 (mandatory) + minisign (mandatory).
# =============================================================================

command -v info >/dev/null 2>&1  || info()  { printf '[INFO]  %s\n' "$*" >&2; }
command -v warn >/dev/null 2>&1  || warn()  { printf '[WARN]  %s\n' "$*" >&2; }
command -v error >/dev/null 2>&1 || error() { printf '[ERROR] %s\n' "$*" >&2; }

ados_verify_sha256() {
    local artifact="$1" base dir
    base="$(basename "${artifact}")"
    dir="$(dirname "${artifact}")"
    if [ ! -f "${artifact}.sha256" ]; then
        warn "missing ${base}.sha256"
        return 1
    fi
    ( cd "${dir}" && sha256sum -c "${base}.sha256" >/dev/null 2>&1 )
}

ados_verify_minisign() {
    local artifact="$1" pubkey="$2" base
    base="$(basename "${artifact}")"
    if ! command -v minisign >/dev/null 2>&1; then
        warn "minisign not installed; cannot verify signature of ${base}"
        return 2
    fi
    if [ ! -f "${artifact}.minisig" ]; then
        warn "missing ${base}.minisig"
        return 3
    fi
    if minisign -V -P "${pubkey}" -m "${artifact}" -x "${artifact}.minisig" >/dev/null 2>&1; then
        return 0
    fi
    error "minisign signature INVALID for ${base}"
    return 1
}

ados_verify_artifact() {
    local artifact="$1" pubkey="$2"
    local base rc
    base="$(basename "${artifact}")"

    if ! ados_verify_sha256 "${artifact}"; then
        error "SHA256 verification failed for ${base}"
        return 1
    fi

    if [ -z "${pubkey}" ]; then
        error "no signing key available; refusing unsigned ${base}"
        return 1
    fi

    ados_verify_minisign "${artifact}" "${pubkey}"
    rc=$?
    case "${rc}" in
        0) return 0 ;;
        1)  # Signature present but INVALID — tamper.
            error "tamper check failed for ${base}; refusing to install"
            return 1 ;;
        *)  # 2 (minisign missing) or 3 (no .minisig) — cannot prove origin.
            error "${base} could not be signature-verified; refusing to install"
            return 1 ;;
    esac
}
