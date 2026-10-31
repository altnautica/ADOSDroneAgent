# Device-tree build, install and boot-config snapshot helpers.
#
# Sourced by scripts/drivers/install-display-overlay.sh; defines functions only.
# Globals (paths, board/display ids, log colours) come from the main script.
# shellcheck shell=bash
# shellcheck disable=SC2034,SC2154

# ----------------------------------------------------------------------------
# Build deps (lazy install — only when we are about to compile a DTS)
# ----------------------------------------------------------------------------
ensure_dtc() {
    if command -v dtc >/dev/null 2>&1; then
        return 0
    fi
    info "Installing device-tree-compiler..."
    DEBIAN_FRONTEND=noninteractive apt-get -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold install -y device-tree-compiler
}


# Snapshot a boot-config file before we edit it so the apply-verify-auto-
# revert probe can restore it if the panel never binds. Idempotent: keeps
# the FIRST (pristine) snapshot if one already exists, so re-runs never
# overwrite the known-good baseline with an already-edited file. Records the
# snapshot + live paths in BOOT_CONFIG_* for the probation marker. This is
# what makes a blind overlay apply brick-safe on every board: a boot config
# we touch is always restorable.
snapshot_boot_config() {
    local target="$1"
    [ -f "${target}" ] || return 0
    local bak="${target}.ados-bak"
    BOOT_CONFIG_PATH="${target}"
    if [ -f "${bak}" ]; then
        BOOT_CONFIG_SNAPSHOT="${bak}"
        return 0
    fi
    if cp -f "${target}" "${bak}" 2>/dev/null; then
        BOOT_CONFIG_SNAPSHOT="${bak}"
        info "Boot-config snapshot saved to ${bak}."
        return 0
    fi
    # Fail closed: without a restorable baseline a blind overlay edit could
    # leave the board unbootable with no auto-revert. Refuse the edit instead.
    error "Could not snapshot ${target}; refusing to edit the boot config without a restorable baseline (brick-safety)."
    return 1
}

compile_and_install_repo_dtbo() {
    local board="$1"
    local display="$2"
    local src="${OVERLAY_DIR}/${board}-${display}.dts"
    if [ ! -f "${src}" ]; then
        error "Overlay source missing: ${src}"
        return 2
    fi
    ensure_dtc
    local out="${BUILD_DIR}/${board}-${display}.dtbo"
    info "Compiling ${src} -> ${out}"
    if ! dtc -@ -I dts -O dtb -o "${out}" "${src}" 2>"${BUILD_DIR}/dtc.log"; then
        error "dtc failed:"
        cat "${BUILD_DIR}/dtc.log" >&2
        return 2
    fi
    install -d -m 0755 "${BOOT_DIR}/overlay-user"
    install -m 0644 "${out}" "${BOOT_DIR}/overlay-user/${board}-${display}.dtbo"
    info "Installed ${BOOT_DIR}/overlay-user/${board}-${display}.dtbo"
}

compile_and_install_upstream_dtbo() {
    local overlay_name="$1"
    local src="${UPSTREAM_DIR}/${overlay_name}.dts"
    if [ ! -f "${src}" ]; then
        error "Vendored upstream source missing: ${src}"
        return 2
    fi
    ensure_dtc
    # The upstream DTS uses Rockchip dt-bindings macros (RK_PA3 etc.)
    # which require a cpp pass before dtc. Try to find the kernel
    # headers; fail gracefully if absent.
    local kbuild=""
    if [ -d "/lib/modules/$(uname -r)/build/include" ]; then
        kbuild="/lib/modules/$(uname -r)/build/include"
    fi
    local cpp_inc=()
    [ -n "${kbuild}" ] && cpp_inc+=("-I" "${kbuild}")
    cpp_inc+=("-I" "/usr/include")
    local pre="${BUILD_DIR}/${overlay_name}.cpp.dts"
    if ! cpp -E -x assembler-with-cpp -undef -nostdinc "${cpp_inc[@]}" \
            "${src}" -o "${pre}" 2>"${BUILD_DIR}/cpp.log"; then
        error "cpp pre-process failed (kernel headers may be missing):"
        cat "${BUILD_DIR}/cpp.log" >&2
        return 2
    fi
    local out="${BUILD_DIR}/${overlay_name}.dtbo"
    if ! dtc -@ -I dts -O dtb -o "${out}" "${pre}" 2>"${BUILD_DIR}/dtc.log"; then
        error "dtc failed:"
        cat "${BUILD_DIR}/dtc.log" >&2
        return 2
    fi
    install -d -m 0755 "${BOOT_DIR}/dtbo"
    install -m 0644 "${out}" "${BOOT_DIR}/dtbo/${overlay_name}.dtbo"
    info "Installed ${BOOT_DIR}/dtbo/${overlay_name}.dtbo"
}
