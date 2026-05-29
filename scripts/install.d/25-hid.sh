# shellcheck shell=bash
# =============================================================================
# 25-hid.sh — install the prebuilt human-interface daemons (ados-pic +
# ados-input).
#
# CI builds both binaries from the ados-hid crate as static arm64 binaries and
# publishes them to the rolling 'prebuilt-hid' prerelease; here we fetch each and
# verify it through the shared artifact verifier (SHA256 always; an
# Ed25519/minisign signature is enforced automatically once a key + .minisig are
# published).
#
# Best-effort by design: if a fetch or verification fails (offline, no asset yet,
# non-arm64 host), the install continues and the ados-pic / ados-input units'
# ExecStart shims run the packaged Python services. Placing the binaries on disk
# is inert: the shims run them only when the operator both enabled the flag file
# (/etc/ados/hid-rust-enabled) and the binary is present; otherwise the packaged
# Python services are the default. The native ados-pic reads the front-panel GPIO
# buttons in-process, so enabling its flag masks the packaged ados-buttons unit
# (07-systemd.sh). Idempotent: re-running re-fetches and overwrites.
# =============================================================================

ADOS_HID_RELEASE_BASE="https://github.com/altnautica/ADOSDroneAgent/releases/download/prebuilt-hid"
ADOS_PIC_ASSET="ados-pic-aarch64"
ADOS_INPUT_ASSET="ados-input-aarch64"
# Public key for signature verification. Empty until a signing key is
# provisioned; with it empty the edge channel is SHA256-checked only.
ADOS_HID_PUBKEY="${ADOS_HID_PUBKEY:-}"

# Bring in the shared verifier (resolve relative to this module's location).
if ! command -v ados_verify_artifact >/dev/null 2>&1; then
    _ados_hid_lib="$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" 2>/dev/null && pwd || true)"
    if [ -n "${_ados_hid_lib}" ] && [ -f "${_ados_hid_lib}/verify.sh" ]; then
        # shellcheck source=/dev/null
        . "${_ados_hid_lib}/verify.sh"
    fi
fi

# Fetch + verify + install one prebuilt asset into INSTALL_DIR/bin. Returns 0
# even on failure (best-effort), printing a warning. Echoes nothing.
_install_hid_asset() {
    local asset="$1"
    local bin_dir="${INSTALL_DIR}/bin"
    local dest="${bin_dir}/${asset%-aarch64}"
    local tmp
    tmp="$(mktemp -d)"
    install -d -m 0755 "${bin_dir}"

    if ! curl -fsSL "${ADOS_HID_RELEASE_BASE}/${asset}" -o "${tmp}/${asset}" \
        || ! curl -fsSL "${ADOS_HID_RELEASE_BASE}/${asset}.sha256" -o "${tmp}/${asset}.sha256"; then
        warn "Could not fetch the ${asset} prebuilt; skipping."
        rm -rf "${tmp}"
        return 0
    fi
    curl -fsSL "${ADOS_HID_RELEASE_BASE}/${asset}.minisig" -o "${tmp}/${asset}.minisig" 2>/dev/null || true

    if command -v ados_verify_artifact >/dev/null 2>&1; then
        if ! ados_verify_artifact "${tmp}/${asset}" "${ADOS_HID_PUBKEY}" "edge" 0; then
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
    info "HID binary installed: ${dest}"
}

install_hid_binary() {
    local arch
    arch="$(uname -m)"
    if [ "${arch}" != "aarch64" ] && [ "${arch}" != "arm64" ]; then
        warn "ados-hid prebuilts are arm64 only; skipping on ${arch}."
        return 0
    fi

    _install_hid_asset "${ADOS_PIC_ASSET}"
    _install_hid_asset "${ADOS_INPUT_ASSET}"

    # The native binaries are OPT-IN: re-assert the running services onto them
    # ONLY when the operator has enabled the flag. Without the flag the units
    # keep running the Python services, so a routine upgrade never bounces the
    # arbiter or input manager for binaries they would not select anyway.
    if [ -e "${CONFIG_DIR}/hid-rust-enabled" ]; then
        if systemctl is-active --quiet ados-pic 2>/dev/null; then
            systemctl restart ados-pic 2>/dev/null || true
            info "PIC arbiter restarted onto the installed binary (flag enabled)."
        fi
        if systemctl is-active --quiet ados-input 2>/dev/null; then
            systemctl restart ados-input 2>/dev/null || true
            info "Input manager restarted onto the installed binary (flag enabled)."
        fi
    fi
}
export -f install_hid_binary
