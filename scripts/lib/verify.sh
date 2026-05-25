# shellcheck shell=bash
# =============================================================================
# verify.sh — artifact integrity + authenticity for the full agent.
#
# Brings signed-artifact verification to the full agent install (prebuilt
# kernel modules, and the stable-channel wheel + deploy bundle). Sourceable,
# side-effect-free. SHA256 is always required; an Ed25519 (minisign)
# signature is required on the stable channel and whenever a real public
# key is supplied. A tampered signature is fatal on every channel; a merely
# unverifiable artifact (no signature, or minisign absent) is fatal on
# stable and tolerated on edge with the SHA256 check still enforced.
#
# Functions (all return 0/non-zero, never exit — callers decide fatality):
#   ados_verify_sha256 ARTIFACT
#       Verify ARTIFACT against ARTIFACT.sha256 (sha256sum -c).
#   ados_verify_minisign ARTIFACT PUBKEY
#       Verify ARTIFACT against ARTIFACT.minisig. Return codes:
#         0 verified | 1 signature INVALID (tamper) | 2 minisign missing |
#         3 signature file missing.
#   ados_verify_artifact ARTIFACT PUBKEY CHANNEL [ALLOW_UNSIGNED]
#       Orchestrate SHA256 (mandatory) + minisign. CHANNEL is "stable" or
#       "edge". ALLOW_UNSIGNED=1 force-skips the signature on any channel
#       (SHA256 still enforced).
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
    local artifact="$1" pubkey="$2" channel="${3:-edge}" allow_unsigned="${4:-0}"
    local base rc
    base="$(basename "${artifact}")"

    # SHA256 is always mandatory, on every channel.
    if ! ados_verify_sha256 "${artifact}"; then
        error "SHA256 verification failed for ${base}"
        return 1
    fi

    if [ "${allow_unsigned}" = "1" ]; then
        warn "allow-unsigned set; skipping signature check for ${base}"
        return 0
    fi

    # No signing key provisioned yet (CI has not substituted a real key).
    if [ -z "${pubkey}" ]; then
        if [ "${channel}" = "stable" ]; then
            error "no signing key available; refusing unsigned ${base} on stable channel"
            return 1
        fi
        warn "no signing key; ${base} is SHA256-checked only (edge channel)"
        return 0
    fi

    ados_verify_minisign "${artifact}" "${pubkey}"
    rc=$?
    case "${rc}" in
        0) return 0 ;;
        1)  # Signature present but INVALID — tamper. Fatal everywhere.
            error "tamper check failed for ${base}; refusing to install"
            return 1 ;;
        *)  # 2 (minisign missing) or 3 (no .minisig) — unverifiable, not tampered.
            if [ "${channel}" = "stable" ]; then
                error "${base} could not be signature-verified on stable channel"
                return 1
            fi
            warn "${base} signature unverifiable; SHA256-checked only (edge channel)"
            return 0 ;;
    esac
}
